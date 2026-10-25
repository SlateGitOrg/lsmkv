//! `lsmkv` — a small command-line client for an on-disk database.
//!
//! ```text
//! lsmkv put  <dir> <key> <value>
//! lsmkv get  <dir> <key>
//! lsmkv del  <dir> <key>
//! lsmkv scan <dir> [start] [end]
//! lsmkv load <dir> <count>        # bulk-load count synthetic keys
//! lsmkv compact <dir>
//! lsmkv stats <dir>
//! ```
//! Set `LSMKV_LOG=info` to see flushes, compactions and recovery.

use lsmkv::{Db, Options, ReadOptions, WriteBatch, WriteOptions};
use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!(
        "usage: lsmkv <put|get|del|scan|load|compact|stats> <dir> [args...]\n\
         \n  put <dir> <key> <value>   set a key (fsynced)\
         \n  get <dir> <key>           print a value\
         \n  del <dir> <key>           delete a key (fsynced)\
         \n  scan <dir> [start] [end]  list keys in [start, end)\
         \n  load <dir> <count>        bulk-load synthetic keys key000000..\
         \n  compact <dir>             full manual compaction\
         \n  stats <dir>               level layout and counters"
    );
    ExitCode::from(2)
}

fn run(args: &[String]) -> lsmkv::Result<ExitCode> {
    let (Some(cmd), Some(dir)) = (args.first(), args.get(1)) else {
        return Ok(usage());
    };
    const COMMANDS: [&str; 7] = ["put", "get", "del", "scan", "load", "compact", "stats"];
    if !COMMANDS.contains(&cmd.as_str()) {
        return Ok(usage());
    }
    let db = Db::open(dir, Options::default())?;
    let sync = WriteOptions { sync: true };
    match (cmd.as_str(), &args[2..]) {
        ("put", [k, v]) => {
            let mut b = WriteBatch::new();
            b.put(k.as_bytes(), v.as_bytes());
            db.write(&sync, b)?;
        }
        ("del", [k]) => {
            let mut b = WriteBatch::new();
            b.delete(k.as_bytes());
            db.write(&sync, b)?;
        }
        ("get", [k]) => match db.get(k.as_bytes())? {
            Some(v) => println!("{}", String::from_utf8_lossy(&v)),
            None => {
                eprintln!("(not found)");
                return Ok(ExitCode::from(1));
            }
        },
        ("scan", rest) if rest.len() <= 2 => {
            let start = rest.first().map(|s| s.as_bytes());
            let end = rest.get(1).map(|s| s.as_bytes());
            for item in db.scan(&ReadOptions::default(), start, end)? {
                let (k, v) = item?;
                println!(
                    "{}\t{}",
                    String::from_utf8_lossy(&k),
                    String::from_utf8_lossy(&v)
                );
            }
        }
        ("load", [n]) => {
            let n: u64 = n
                .parse()
                .map_err(|_| lsmkv::Error::invalid("count must be a number"))?;
            let mut b = WriteBatch::new();
            for i in 0..n {
                b.put(
                    format!("key{i:06}").as_bytes(),
                    format!("value-{i}").as_bytes(),
                );
                if b.count() == 1000 {
                    db.write(&WriteOptions::default(), std::mem::take(&mut b))?;
                }
            }
            db.write(&sync, b)?;
            println!("loaded {n} keys");
        }
        ("compact", []) => {
            db.compact_range(None, None)?;
            print!("{}", db.describe_levels());
        }
        ("stats", []) => {
            db.wait_for_compactions()?;
            print!("{}", db.describe_levels());
            println!("last sequence: {}", db.last_sequence());
        }
        _ => return Ok(usage()),
    }
    db.close()?;
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    lsmkv::util::logger::init_from_env("LSMKV_LOG");
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}
