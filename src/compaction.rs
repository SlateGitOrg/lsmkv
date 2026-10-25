//! Leveled compaction: choosing what to compact and the per-key rules used
//! while merging.
//!
//! * **Scoring.** L0 scores `files / l0_compaction_trigger`; level `n >= 1`
//!   scores `bytes / (base · multiplier^(n−1))`. The highest score >= 1 wins.
//! * **Input selection.** For L0, every L0 file overlapping the chosen one
//!   (transitively). For `n >= 1`, the first file after the level's
//!   round-robin *compact pointer*, so successive compactions sweep the key
//!   space. All overlapping files of level `n+1` join, and level `n` is grown
//!   if that does not pull in more of level `n+1`.
//! * **Output splitting.** Outputs are cut at `max_file_size` and whenever an
//!   output would overlap more than `10 · max_file_size` of level `n+2`
//!   (the *grandparents*), bounding the cost of the next compaction.
//! * **Trivial move.** One input file and no overlap below: re-parent the file
//!   without rewriting it.

use crate::key::{compare_internal, user_key};
use crate::options::{Options, NUM_LEVELS};
use crate::version::{FileMetaData, Version, VersionEdit, VersionSet};
use std::sync::Arc;

/// A planned compaction of `level` into `level + 1`.
pub struct Compaction {
    /// Source level.
    pub level: usize,
    /// `inputs[0]` from `level`, `inputs[1]` from `level + 1`.
    pub inputs: [Vec<Arc<FileMetaData>>; 2],
    /// Overlapping files in `level + 2`.
    pub grandparents: Vec<Arc<FileMetaData>>,
    /// Size at which outputs are cut.
    pub max_output_file_size: u64,
    /// Edit accumulating the compaction's metadata changes.
    pub edit: VersionEdit,
    input_version: Arc<Version>,
    /// Manual compactions always rewrite so they can purge garbage.
    allow_trivial_move: bool,
    max_grandparent_overlap: u64,
    grandparent_index: usize,
    seen_key: bool,
    overlapped_bytes: u64,
    level_ptrs: [usize; NUM_LEVELS],
}

fn total_size(files: &[Arc<FileMetaData>]) -> u64 {
    files.iter().map(|f| f.size).sum()
}

/// Smallest and largest internal keys over `files`.
fn key_range(files: &[&Arc<FileMetaData>]) -> (Vec<u8>, Vec<u8>) {
    let mut smallest = files[0].smallest.clone();
    let mut largest = files[0].largest.clone();
    for f in &files[1..] {
        if compare_internal(&f.smallest, &smallest).is_lt() {
            smallest = f.smallest.clone();
        }
        if compare_internal(&f.largest, &largest).is_gt() {
            largest = f.largest.clone();
        }
    }
    (smallest, largest)
}

impl Compaction {
    fn new(level: usize, version: Arc<Version>, opts: &Options) -> Self {
        Compaction {
            level,
            inputs: [Vec::new(), Vec::new()],
            grandparents: Vec::new(),
            max_output_file_size: opts.max_file_size,
            edit: VersionEdit::default(),
            input_version: version,
            allow_trivial_move: true,
            max_grandparent_overlap: 10 * opts.max_file_size,
            grandparent_index: 0,
            seen_key: false,
            overlapped_bytes: 0,
            level_ptrs: [0; NUM_LEVELS],
        }
    }

    /// Total input files.
    pub fn num_input_files(&self) -> usize {
        self.inputs[0].len() + self.inputs[1].len()
    }

    /// Total input bytes.
    pub fn input_bytes(&self) -> u64 {
        total_size(&self.inputs[0]) + total_size(&self.inputs[1])
    }

    /// The version the inputs were chosen from.
    pub fn input_version(&self) -> &Arc<Version> {
        &self.input_version
    }

    /// True if the single input can be moved down without rewriting.
    pub fn is_trivial_move(&self) -> bool {
        self.allow_trivial_move
            && self.inputs[0].len() == 1
            && self.inputs[1].is_empty()
            && total_size(&self.grandparents) <= self.max_grandparent_overlap
    }

    /// Adds deletion records for every input file to the edit.
    pub fn add_input_deletions(&mut self) {
        for (which, files) in self.inputs.iter().enumerate() {
            for f in files {
                self.edit.delete_file(self.level + which, f.number);
            }
        }
    }

    /// True if no level below `level + 1` can contain `uk`, so a tombstone
    /// for it has nothing left to shadow. Keys must be passed in ascending
    /// order (the pointers only move forward).
    pub fn is_base_level_for_key(&mut self, uk: &[u8]) -> bool {
        for lvl in self.level + 2..NUM_LEVELS {
            let files = &self.input_version.files[lvl];
            let ptr = &mut self.level_ptrs[lvl];
            while *ptr < files.len() {
                let f = &files[*ptr];
                if uk <= f.largest_user() {
                    if uk >= f.smallest_user() {
                        return false;
                    }
                    break;
                }
                *ptr += 1;
            }
        }
        true
    }

    /// True if the current output should be closed before adding `ikey`,
    /// because it already overlaps too many grandparent bytes.
    pub fn should_stop_before(&mut self, ikey: &[u8]) -> bool {
        while self.grandparent_index < self.grandparents.len()
            && compare_internal(ikey, &self.grandparents[self.grandparent_index].largest).is_gt()
        {
            if self.seen_key {
                self.overlapped_bytes += self.grandparents[self.grandparent_index].size;
            }
            self.grandparent_index += 1;
        }
        self.seen_key = true;
        if self.overlapped_bytes > self.max_grandparent_overlap {
            self.overlapped_bytes = 0;
            return true;
        }
        false
    }
}

/// Fills `inputs[1]` and `grandparents`, growing `inputs[0]` when free.
fn setup_other_inputs(c: &mut Compaction) {
    let level = c.level;
    let v = c.input_version.clone();
    let (smallest, largest) = key_range(&c.inputs[0].iter().collect::<Vec<_>>());
    c.inputs[1] = v.overlapping_inputs(
        level + 1,
        Some(user_key(&smallest)),
        Some(user_key(&largest)),
    );
    let all: Vec<&Arc<FileMetaData>> = c.inputs[0].iter().chain(c.inputs[1].iter()).collect();
    let (all_smallest, all_largest) = key_range(&all);

    // Try to include more level-`level` files without changing inputs[1].
    if !c.inputs[1].is_empty() {
        let expanded0 = v.overlapping_inputs(
            level,
            Some(user_key(&all_smallest)),
            Some(user_key(&all_largest)),
        );
        let limit = 25 * c.max_output_file_size;
        if expanded0.len() > c.inputs[0].len()
            && total_size(&expanded0) + total_size(&c.inputs[1]) < limit
        {
            let (s, l) = key_range(&expanded0.iter().collect::<Vec<_>>());
            let expanded1 = v.overlapping_inputs(level + 1, Some(user_key(&s)), Some(user_key(&l)));
            if expanded1.len() == c.inputs[1].len() {
                log::debug!(
                    "expanding L{level} inputs {}+{} -> {}+{}",
                    c.inputs[0].len(),
                    c.inputs[1].len(),
                    expanded0.len(),
                    expanded1.len()
                );
                c.inputs[0] = expanded0;
                c.inputs[1] = expanded1;
            }
        }
    }
    let all: Vec<&Arc<FileMetaData>> = c.inputs[0].iter().chain(c.inputs[1].iter()).collect();
    let (all_smallest, all_largest) = key_range(&all);
    if level + 2 < NUM_LEVELS {
        c.grandparents = v.overlapping_inputs(
            level + 2,
            Some(user_key(&all_smallest)),
            Some(user_key(&all_largest)),
        );
    }
    // Advance the round-robin cursor past this compaction's inputs.
    let (_, largest0) = key_range(&c.inputs[0].iter().collect::<Vec<_>>());
    c.edit.compact_pointers.push((level, largest0));
}

/// Chooses the most urgent size-triggered compaction, if any.
pub fn pick_compaction(vs: &VersionSet, opts: &Options) -> Option<Compaction> {
    let v = vs.current();
    if v.compaction_score < 1.0 {
        return None;
    }
    let level = v.compaction_level;
    let mut c = Compaction::new(level, v.clone(), opts);
    let pointer = vs.compact_pointer(level);
    let first = v.files[level]
        .iter()
        .find(|f| pointer.is_empty() || compare_internal(&f.largest, pointer).is_gt())
        .or_else(|| v.files[level].first())?
        .clone();
    c.inputs[0] = vec![first];
    if level == 0 {
        let f = &c.inputs[0][0];
        let (lo, hi) = (f.smallest_user().to_vec(), f.largest_user().to_vec());
        c.inputs[0] = v.overlapping_inputs(0, Some(&lo), Some(&hi));
    }
    setup_other_inputs(&mut c);
    Some(c)
}

/// Builds a manual compaction of `level` restricted to a user-key range.
pub fn compact_range(
    vs: &VersionSet,
    opts: &Options,
    level: usize,
    begin: Option<&[u8]>,
    end: Option<&[u8]>,
) -> Option<Compaction> {
    let v = vs.current();
    let inputs = v.overlapping_inputs(level, begin, end);
    if inputs.is_empty() || level + 1 >= NUM_LEVELS {
        return None;
    }
    let mut c = Compaction::new(level, v, opts);
    c.allow_trivial_move = false;
    c.inputs[0] = inputs;
    setup_other_inputs(&mut c);
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::{FileSystem, SimFs};
    use crate::key::{internal_key, ValueKind};
    use crate::stats::Stats;
    use crate::version::TableCache;
    use std::path::PathBuf;

    fn meta(n: u64, lo: &str, hi: &str, size: u64) -> FileMetaData {
        FileMetaData {
            number: n,
            size,
            smallest: internal_key(lo.as_bytes(), 9, ValueKind::Value),
            largest: internal_key(hi.as_bytes(), 1, ValueKind::Value),
        }
    }

    fn set_with(files: &[(usize, FileMetaData)], opts: &Options) -> VersionSet {
        let fs: Arc<dyn FileSystem> = Arc::new(SimFs::new());
        let dir = PathBuf::from("/db");
        let tc = Arc::new(TableCache::new(
            fs.clone(),
            dir.clone(),
            opts,
            Arc::new(Stats::default()),
        ));
        let mut vs = VersionSet::new(fs, dir, opts.clone(), tc);
        let mut e = VersionEdit::default();
        for (l, f) in files {
            vs.mark_file_number_used(f.number);
            e.add_file(*l, f.clone());
        }
        vs.log_and_apply(e).unwrap();
        vs
    }

    #[test]
    fn l0_trigger_picks_all_overlapping_l0_files() {
        let opts = Options::for_testing();
        let files = vec![
            (0, meta(1, "a", "c", 100)),
            (0, meta(2, "b", "d", 100)),
            (0, meta(3, "x", "z", 100)),
            (0, meta(4, "c", "e", 100)),
            (1, meta(5, "d", "f", 100)),
            (1, meta(6, "m", "n", 100)),
            (2, meta(7, "a", "b", 100)),
        ];
        let vs = set_with(&files, &opts);
        let c = pick_compaction(&vs, &opts).expect("L0 is at trigger");
        assert_eq!(c.level, 0);
        let mut nums: Vec<u64> = c.inputs[0].iter().map(|f| f.number).collect();
        nums.sort();
        assert_eq!(nums, vec![1, 2, 4]);
        assert_eq!(
            c.inputs[1].iter().map(|f| f.number).collect::<Vec<_>>(),
            vec![5]
        );
        assert_eq!(c.grandparents.len(), 1);
        assert_eq!(c.num_input_files(), 4);
        assert_eq!(c.input_bytes(), 400);
        assert!(!c.is_trivial_move());
    }

    #[test]
    fn size_trigger_round_robin_and_trivial_move() {
        let opts = Options::for_testing();
        let big = opts.max_bytes_for_level(1);
        let files = vec![(1, meta(1, "a", "b", big)), (1, meta(2, "c", "d", big))];
        let mut vs = set_with(&files, &opts);
        let c = pick_compaction(&vs, &opts).unwrap();
        assert_eq!(c.level, 1);
        assert_eq!(c.inputs[0][0].number, 1);
        assert!(c.is_trivial_move());
        // Commit the compact pointer; the next pick takes the following file.
        vs.log_and_apply(c.edit).unwrap();
        let c = pick_compaction(&vs, &opts).unwrap();
        assert_eq!(c.inputs[0][0].number, 2);
        vs.log_and_apply(c.edit).unwrap();
        // Pointer past the end wraps around.
        let c = pick_compaction(&vs, &opts).unwrap();
        assert_eq!(c.inputs[0][0].number, 1);
    }

    #[test]
    fn nothing_to_do_and_manual_range() {
        let opts = Options::for_testing();
        let vs = set_with(
            &[(1, meta(1, "a", "b", 10)), (2, meta(2, "a", "z", 10))],
            &opts,
        );
        assert!(pick_compaction(&vs, &opts).is_none());
        let c = compact_range(&vs, &opts, 1, None, None).unwrap();
        assert_eq!(c.inputs[1][0].number, 2);
        let c = compact_range(&vs, &opts, 2, None, None).unwrap();
        assert!(!c.is_trivial_move(), "manual compactions rewrite");
        assert!(compact_range(&vs, &opts, 3, None, None).is_none());
        assert!(compact_range(&vs, &opts, 1, Some(b"x"), Some(b"y")).is_none());
    }

    #[test]
    fn base_level_and_grandparent_cuts() {
        let opts = Options {
            max_file_size: 1024,
            ..Options::for_testing()
        };
        let files = vec![
            (1, meta(1, "a", "z", 100)),
            (3, meta(2, "c", "d", 5000)),
            (3, meta(3, "m", "n", 5000)),
            (3, meta(4, "p", "q", 5000)),
        ];
        let vs = set_with(&files, &opts);
        let mut c = compact_range(&vs, &opts, 1, None, None).unwrap();
        assert!(c.is_base_level_for_key(b"a"));
        assert!(!c.is_base_level_for_key(b"c"));
        assert!(c.is_base_level_for_key(b"e"));
        assert!(!c.is_base_level_for_key(b"m"));
        assert!(c.is_base_level_for_key(b"zz"));

        // Grandparents at level 3 only matter for level-1 compactions'
        // outputs at level 2 when compacting level 1 -> 2: here level+2 = 3.
        let ik = |s: &str| internal_key(s.as_bytes(), 1, ValueKind::Value);
        assert_eq!(c.grandparents.len(), 3);
        assert!(!c.should_stop_before(&ik("a")));
        assert!(!c.should_stop_before(&ik("e"))); // passed 5000 bytes: not > 10240
        assert!(!c.should_stop_before(&ik("o")));
        assert!(c.should_stop_before(&ik("r"))); // 15000 > 10240
        assert!(!c.should_stop_before(&ik("s")));
    }
}
