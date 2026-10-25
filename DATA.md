# Data

lsmkv uses **no external datasets**. Every test, benchmark and demo input is
generated at run time by a deterministic seeded PRNG (`src/util/rng.rs`,
SplitMix64):

| Where | What is generated |
|---|---|
| `tests/common/mod.rs` | write workloads (puts, deletes, multi-op batches) over small key spaces |
| `tests/model_proptest.rs` | random operation sequences from `proptest` strategies |
| `src/bin/db_bench.rs` | 16-byte decimal keys, 1 MiB pool of random value bytes |
| `examples/demo.rs` | `key0000000`-style keys with padded string values |

Checksum and hash test vectors in `src/util/crc32c.rs` (RFC 3720 §B.4) and
`src/util/hash.rs` (LevelDB `util/hash_test.cc`) are short published
constants embedded in the tests.

Nothing is downloaded; `bench/RESULTS.md` is the only committed data artifact
and it is produced by `scripts/bench.sh`.
