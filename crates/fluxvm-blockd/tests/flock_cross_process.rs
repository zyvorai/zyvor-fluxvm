// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

//! An overlay another process holds open refuses resize, rebase and branch.
//!
//! This lives in its own test binary on purpose. It spawns a child process, and a fork copies every file
//! descriptor the other threads of the process have open, flocks included, until the child execs. Inside the
//! unit-test binary that made unrelated tests that take an exclusive lock fail now and then with "overlay is in
//! use". Here nothing else runs in the process.

use fluxvm_blockd::{BlockBackend, CowBackend, OpenOptions};
use std::io::{self, BufRead};

const BLOCK_SIZE: u64 = fluxvm_blockd::BLOCK_SIZE;

#[test]
fn resize_and_rebase_refuse_while_overlay_is_open() {
    // Hold the shared flock in another process: same-process SH→EX upgrade can
    // block forever on Darwin even with LOCK_NB.
    let d = tempfile::tempdir().unwrap();
    let base = d.path().join("base.img");
    std::fs::write(&base, vec![0xABu8; 2 * BLOCK_SIZE as usize]).unwrap();
    let ov = d.path().join("busy.ovl");
    {
        let held = CowBackend::create(&ov, &base, None).unwrap();
        held.write_at(0, b"xx").unwrap();
        held.flush().unwrap();
    }
    // The holder says "locked" once it really holds the flock. Sleeping a fixed 200 ms instead raced interpreter
    // start-up: on a loaded runner python3 had not locked yet, resize_file succeeded, and the unwrap_err below
    // panicked ("called `Result::unwrap_err()` on an `Ok` value: false").
    let mut holder = std::process::Command::new("python3")
        .args([
            "-c",
            "import fcntl,sys,time; f=open(sys.argv[1],'r+b'); fcntl.flock(f, fcntl.LOCK_SH); print('locked', flush=True); time.sleep(30)",
            ov.to_str().unwrap(),
        ])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("python3 flock holder");
    let mut ready = String::new();
    std::io::BufReader::new(holder.stdout.take().unwrap())
        .read_line(&mut ready)
        .expect("read the flock holder's readiness line");
    assert_eq!(ready.trim(), "locked", "flock holder did not take the lock");
    let e = CowBackend::resize_file(&ov, 4 * BLOCK_SIZE).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
    assert!(e.to_string().contains("in use"), "{e}");
    let e = CowBackend::rebase(&ov, base.to_str().unwrap(), &OpenOptions::default()).unwrap_err();
    assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
    let branch = d.path().join("child.ovl");
    match CowBackend::create_on_overlay(&branch, &ov) {
        Err(e) => {
            assert_eq!(e.kind(), io::ErrorKind::WouldBlock, "{e}");
            assert!(e.to_string().contains("in use"), "{e}");
        }
        Ok(_) => panic!("branch while open should refuse"),
    }
    let _ = holder.kill();
    let _ = holder.wait();
    assert!(!CowBackend::resize_file(&ov, 4 * BLOCK_SIZE).unwrap());
    CowBackend::rebase(&ov, base.to_str().unwrap(), &OpenOptions::default()).unwrap();
    CowBackend::create_on_overlay(&branch, &ov).unwrap();
}
