// Copyright 2026 Zyvor AI Labs · https://zyvor.dev
// SPDX-License-Identifier: Apache-2.0

use crate::bus::Bus;
use crate::error::Result;
use crate::hypervisor::{VcpuBackend, VcpuExit};
use std::sync::Arc;

pub fn vcpu_loop(id: u8, mut vcpu: Box<dyn VcpuBackend>, bus: Arc<Bus>) -> Result<()> {
    eprintln!("[vcpu-{id}] start");
    loop {
        match vcpu.run()? {
            VcpuExit::IoOut { port, data } => bus.pio_write(port, &data)?,
            VcpuExit::IoIn { port, size } => {
                let mut buf = vec![0u8; size as usize];
                bus.pio_read(port, &mut buf)?;
                vcpu.complete_read(&buf)?;
            }
            VcpuExit::MmioWrite { addr, data } => {
                let _ = bus.mmio_write(addr, &data);
            }
            VcpuExit::MmioRead { addr, size } => {
                let mut buf = vec![0u8; size as usize];
                if let Err(e) = bus.mmio_read(addr, &mut buf) {
                    // Reads of a failing device return all-ones, like an unmapped address.
                    eprintln!("[vcpu-{id}] mmio read {addr:#x}: {e}");
                    buf.fill(0xff);
                }
                vcpu.complete_read(&buf)?;
            }
            VcpuExit::Halt | VcpuExit::Shutdown => {
                eprintln!("[vcpu-{id}] halt/shutdown");
                break;
            }
            VcpuExit::Interrupted => continue,
            VcpuExit::Unknown(s) => eprintln!("[vcpu-{id}] {s}"),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::Bus;
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    struct Scripted {
        exits: VecDeque<VcpuExit>,
        delivered: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl VcpuBackend for Scripted {
        fn set_rip(&mut self, _rip: u64) -> Result<()> {
            Ok(())
        }
        fn run(&mut self) -> Result<VcpuExit> {
            Ok(self.exits.pop_front().unwrap_or(VcpuExit::Halt))
        }
        fn complete_read(&mut self, data: &[u8]) -> Result<()> {
            self.delivered.lock().unwrap().push(data.to_vec());
            Ok(())
        }
    }

    #[test]
    fn io_and_mmio_reads_reach_the_guest() {
        // No device mapped: the bus answers 0xff for both reads.
        let bus = Arc::new(Bus::new());
        let delivered = Arc::new(Mutex::new(Vec::new()));
        let vcpu = Scripted {
            exits: VecDeque::from([
                VcpuExit::IoIn {
                    port: 0x3f8,
                    size: 1,
                },
                VcpuExit::MmioRead {
                    addr: 0xd000_0000,
                    size: 4,
                },
            ]),
            delivered: delivered.clone(),
        };
        vcpu_loop(0, Box::new(vcpu), bus).unwrap();
        assert_eq!(*delivered.lock().unwrap(), vec![vec![0xff], vec![0xff; 4]]);
    }
}
