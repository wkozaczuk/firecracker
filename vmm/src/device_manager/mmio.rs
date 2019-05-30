// Copyright 2018 Amazon.com, Inc. or its affiliates. All Rights Reserved.
// SPDX-License-Identifier: Apache-2.0
//
// Portions Copyright 2017 The Chromium OS Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the THIRD-PARTY file.

use std::collections::HashMap;
use std::os::unix::io::AsRawFd;
use std::sync::{Arc, Mutex};
use std::{fmt, io};

#[cfg(target_arch = "aarch64")]
use arch::aarch64::DeviceInfoForFDT;
use arch::DeviceType;
use devices;
use kernel_cmdline;
use kvm_ioctls::{IoEventAddress, VmFd};
use memory_model::GuestMemory;

/// Errors for MMIO device manager.
#[derive(Debug)]
pub enum Error {
    /// Failed to perform an operation on the bus.
    BusError(devices::BusError),
    /// Could not create the mmio device to wrap a VirtioDevice.
    CreateMmioDevice(io::Error),
    /// Appending to kernel command line failed.
    Cmdline(kernel_cmdline::Error),
    /// Failure in creating or cloning an event fd.
    EventFd(io::Error),
    /// No more IRQs are available.
    IrqsExhausted,
    /// Registering an IO Event failed.
    RegisterIoEvent(io::Error),
    /// Registering an IRQ FD failed.
    RegisterIrqFd(io::Error),
    /// Failed to update the mmio device.
    UpdateFailed,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Error::BusError(ref e) => write!(f, "failed to perform bus operation: {}", e),
            Error::CreateMmioDevice(ref e) => write!(f, "failed to create mmio device: {}", e),
            Error::Cmdline(ref e) => {
                write!(f, "unable to add device to kernel command line: {}", e)
            }
            Error::EventFd(ref e) => write!(f, "failed to create or clone event descriptor: {}", e),
            Error::IrqsExhausted => write!(f, "no more IRQs are available"),
            Error::RegisterIoEvent(ref e) => write!(f, "failed to register IO event: {}", e),
            Error::RegisterIrqFd(ref e) => write!(f, "failed to register irqfd: {}", e),
            Error::UpdateFailed => write!(f, "failed to update the mmio device"),
        }
    }
}

type Result<T> = ::std::result::Result<T, Error>;

/// This represents the size of the mmio device specified to the kernel as a cmdline option
/// It has to be larger than 0x100 (the offset where the configuration space starts from
/// the beginning of the memory mapped device registers) + the size of the configuration space
/// Currently hardcoded to 4K.
const MMIO_LEN: u64 = 0x1000;

/// This represents the offset at which the device should call BusDevice::write in order to write
/// to its configuration space.
const MMIO_CFG_SPACE_OFF: u64 = 0x100;

/// Manages the complexities of registering a MMIO device.
pub struct MMIODeviceManager {
    pub bus: devices::Bus,
    guest_mem: GuestMemory,
    mmio_base: u64,
    irq: u32,
    last_irq: u32,
    id_to_dev_info: HashMap<String, MMIODeviceInfo>,
}

impl MMIODeviceManager {
    /// Create a new DeviceManager handling mmio devices (virtio net, block).
    pub fn new(
        guest_mem: GuestMemory,
        mmio_base: u64,
        irq_interval: (u32, u32),
    ) -> MMIODeviceManager {
        MMIODeviceManager {
            guest_mem,
            mmio_base,
            irq: irq_interval.0,
            last_irq: irq_interval.1,
            bus: devices::Bus::new(),
            id_to_dev_info: HashMap::new(),
        }
    }

    /// Register a virtio device to be used via MMIO transport.
    pub fn register_virtio_device(
        &mut self,
        vm: &VmFd,
        device: Box<devices::virtio::VirtioDevice>,
        cmdline: &mut kernel_cmdline::Cmdline,
        id: &str,
    ) -> Result<u64> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }
        let mmio_device = devices::virtio::MmioDevice::new(self.guest_mem.clone(), device)
            .map_err(Error::CreateMmioDevice)?;
        for (i, queue_evt) in mmio_device.queue_evts().iter().enumerate() {
            let io_addr = IoEventAddress::Mmio(
                self.mmio_base + u64::from(devices::virtio::NOTIFY_REG_OFFSET),
            );

            vm.register_ioevent(queue_evt.as_raw_fd(), &io_addr, i as u32)
                .map_err(Error::RegisterIoEvent)?;
        }

        if let Some(interrupt_evt) = mmio_device.interrupt_evt() {
            vm.register_irqfd(interrupt_evt.as_raw_fd(), self.irq)
                .map_err(Error::RegisterIrqFd)?;
        }

        self.bus
            .insert(Arc::new(Mutex::new(mmio_device)), self.mmio_base, MMIO_LEN)
            .map_err(Error::BusError)?;

        // as per doc, [virtio_mmio.]device=<size>@<baseaddr>:<irq> needs to be appended
        // to kernel commandline for virtio mmio devices to get recognized
        // the size parameter has to be transformed to KiB, so dividing hexadecimal value in
        // bytes to 1024; further, the '{}' formatting rust construct will automatically
        // transform it to decimal

        #[cfg(target_arch = "x86_64")]
        cmdline
            .insert(
                "virtio_mmio.device",
                &format!("{}K@0x{:08x}:{}", MMIO_LEN / 1024, self.mmio_base, self.irq),
            )
            .map_err(Error::Cmdline)?;
        let ret = self.mmio_base;

        self.id_to_dev_info.insert(
            id.to_string(),
            MMIODeviceInfo {
                addr: ret,
                len: MMIO_LEN,
                irq: self.irq,
                type_: DeviceType::Virtio,
            },
        );

        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(ret)
    }

    #[cfg(target_arch = "aarch64")]
    /// Register an early console at some MMIO address.
    pub fn register_mmio_serial(
        &mut self,
        vm: &VmFd,
        cmdline: &mut kernel_cmdline::Cmdline,
    ) -> Result<()> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        let com_evt = sys_util::EventFd::new().map_err(Error::EventFd)?;
        let device = devices::legacy::Serial::new_out(
            com_evt.try_clone().map_err(Error::EventFd)?,
            Box::new(io::stdout()),
            Some(4),
        );

        vm.register_irqfd(com_evt.as_raw_fd(), self.irq)
            .map_err(Error::RegisterIrqFd)?;

        self.bus
            .insert(Arc::new(Mutex::new(device)), self.mmio_base, MMIO_LEN)
            .map_err(|err| Error::BusError(err))?;

        cmdline
            .insert("earlycon", &format!("uart,mmio32,0x{:08x}", self.mmio_base))
            .map_err(Error::Cmdline)?;

        let ret = self.mmio_base;
        self.id_to_dev_info.insert(
            "uart".to_string(),
            MMIODeviceInfo {
                addr: ret,
                len: MMIO_LEN,
                irq: self.irq,
                type_: DeviceType::Serial,
            },
        );

        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    /// Register a MMIO RTC device.
    pub fn register_mmio_rtc(&mut self, vm: &VmFd) -> Result<()> {
        if self.irq > self.last_irq {
            return Err(Error::IrqsExhausted);
        }

        // Attaching the RTC device.
        let rtc_evt = sys_util::EventFd::new().map_err(Error::EventFd)?;
        let device = devices::legacy::RTC::new(rtc_evt.try_clone().map_err(Error::EventFd)?);
        vm.register_irqfd(rtc_evt.as_raw_fd(), self.irq)
            .map_err(Error::RegisterIrqFd)?;

        self.bus
            .insert(Arc::new(Mutex::new(device)), self.mmio_base, MMIO_LEN)
            .map_err(|err| Error::BusError(err))?;

        let ret = self.mmio_base;
        self.id_to_dev_info.insert(
            "rtc".to_string(),
            MMIODeviceInfo {
                addr: ret,
                len: MMIO_LEN,
                irq: self.irq,
                type_: DeviceType::RTC,
            },
        );

        self.mmio_base += MMIO_LEN;
        self.irq += 1;

        Ok(())
    }

    #[cfg(target_arch = "aarch64")]
    /// Gets the information of the devices registered up to some point in time.
    pub fn get_device_info(&self) -> &HashMap<String, MMIODeviceInfo> {
        &self.id_to_dev_info
    }

    /// Update a drive by rebuilding its config space and rewriting it on the bus.
    pub fn update_drive(&self, addr: u64, new_size: u64) -> Result<()> {
        if let Some((_, device)) = self.bus.get_device(addr) {
            let data = devices::virtio::build_config_space(new_size);
            let mut busdev = device.lock().map_err(|_| Error::UpdateFailed)?;

            busdev.write(MMIO_CFG_SPACE_OFF, &data[..]);
            busdev.interrupt(devices::virtio::VIRTIO_MMIO_INT_CONFIG);

            Ok(())
        } else {
            Err(Error::UpdateFailed)
        }
    }

    /// Gets the address of the specified device on the bus.
    pub fn get_address(&self, id: &str) -> Option<&u64> {
        if let Some(dev_info) = self.id_to_dev_info.get(id) {
            return Some(&dev_info.addr);
        }
        None
    }
}

/// Private structure for storing information about the MMIO device registered at some address on the bus.
#[derive(Clone, Debug)]
pub struct MMIODeviceInfo {
    addr: u64,
    irq: u32,
    len: u64,
    type_: DeviceType,
}

#[cfg(target_arch = "aarch64")]
impl DeviceInfoForFDT for MMIODeviceInfo {
    fn addr(&self) -> u64 {
        self.addr
    }
    fn irq(&self) -> u32 {
        self.irq
    }
    fn length(&self) -> u64 {
        self.len
    }
    fn type_(&self) -> &DeviceType {
        &self.type_
    }
}

#[cfg(test)]
mod tests {
    use super::super::super::vmm_config::instance_info::{InstanceInfo, InstanceState};
    use super::super::super::Vmm;
    use super::*;
    use arch;
    use devices::virtio::{ActivateResult, VirtioDevice};
    use kernel_cmdline;
    use memory_model::{GuestAddress, GuestMemory};
    use std::sync::atomic::AtomicUsize;
    use std::sync::mpsc::channel;
    use std::sync::{Arc, RwLock};
    use sys_util::EventFd;
    const QUEUE_SIZES: &[u16] = &[64];

    impl MMIODeviceManager {
        // Removing the address of a device will generate an error when you try to update the
        // drive. The purpose of this method is to test error scenarios and should otherwise
        // not be used.
        pub fn remove_device_info(&mut self, id: &str) {
            self.id_to_dev_info.remove(id).unwrap();
        }
    }

    #[allow(dead_code)]
    #[derive(Clone)]
    struct DummyDevice {
        dummy: u32,
    }

    impl devices::virtio::VirtioDevice for DummyDevice {
        fn device_type(&self) -> u32 {
            0
        }

        fn queue_max_sizes(&self) -> &[u16] {
            QUEUE_SIZES
        }

        fn ack_features(&mut self, page: u32, value: u32) {
            let _ = page;
            let _ = value;
        }

        fn read_config(&self, offset: u64, data: &mut [u8]) {
            let _ = offset;
            let _ = data;
        }

        fn write_config(&mut self, offset: u64, data: &[u8]) {
            let _ = offset;
            let _ = data;
        }

        #[allow(unused_variables)]
        #[allow(unused_mut)]
        fn activate(
            &mut self,
            mem: GuestMemory,
            interrupt_evt: EventFd,
            status: Arc<AtomicUsize>,
            queues: Vec<devices::virtio::Queue>,
            mut queue_evts: Vec<EventFd>,
        ) -> ActivateResult {
            Ok(())
        }
    }

    fn create_vmm_object() -> Vmm {
        let shared_info = Arc::new(RwLock::new(InstanceInfo {
            state: InstanceState::Uninitialized,
            id: "TEST_ID".to_string(),
            vmm_version: "1.0".to_string(),
        }));

        let (_to_vmm, from_api) = channel();
        Vmm::new(
            shared_info,
            EventFd::new().expect("cannot create eventFD"),
            from_api,
            0,
        )
        .expect("Cannot Create VMM")
    }

    #[test]
    fn register_device() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem = GuestMemory::new(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem, 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));

        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy_box = Box::new(DummyDevice { dummy: 0 });
        let vmm = create_vmm_object();

        assert!(device_manager
            .register_virtio_device(vmm.vm.get_fd(), dummy_box, &mut cmdline, "dummy")
            .is_ok());
    }

    #[test]
    fn register_too_many_devices() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem = GuestMemory::new(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem, 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));

        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy_box = Box::new(DummyDevice { dummy: 0 });
        let vmm = create_vmm_object();
        for _i in arch::IRQ_BASE..=arch::IRQ_MAX {
            device_manager
                .register_virtio_device(vmm.vm.get_fd(), dummy_box.clone(), &mut cmdline, "dummy1")
                .unwrap();
        }
        assert_eq!(
            format!(
                "{}",
                device_manager
                    .register_virtio_device(
                        vmm.vm.get_fd(),
                        dummy_box.clone(),
                        &mut cmdline,
                        "dummy2"
                    )
                    .unwrap_err()
            ),
            "no more IRQs are available".to_string()
        );
    }

    #[test]
    fn test_dummy_device() {
        let mut dummy = DummyDevice { dummy: 0 };
        assert_eq!(dummy.device_type(), 0);
        assert_eq!(dummy.queue_max_sizes(), QUEUE_SIZES);

        // test activate
        let m = GuestMemory::new(&[(GuestAddress(0), 0x1000)]).unwrap();
        let ievt = EventFd::new().unwrap();
        let stat = Arc::new(AtomicUsize::new(0));
        let queue_evts = vec![EventFd::new().unwrap()];
        let result = dummy.activate(m.clone(), ievt, stat, Vec::with_capacity(1), queue_evts);
        assert!(result.is_ok());
    }

    #[test]
    fn test_error_messages() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem = GuestMemory::new(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let device_manager =
            MMIODeviceManager::new(guest_mem, 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let e = Error::Cmdline(
            cmdline
                .insert(
                    "virtio_mmio=device",
                    &format!(
                        "{}K@0x{:08x}:{}",
                        MMIO_LEN / 1024,
                        device_manager.mmio_base,
                        device_manager.irq
                    ),
                )
                .unwrap_err(),
        );
        assert_eq!(
            format!("{}", e),
            format!(
                "unable to add device to kernel command line: {}",
                kernel_cmdline::Error::HasEquals
            ),
        );
        assert_eq!(
            format!("{}", Error::UpdateFailed),
            "failed to update the mmio device"
        );
        assert_eq!(
            format!("{}", Error::BusError(devices::BusError::Overlap)),
            format!(
                "failed to perform bus operation: {}",
                devices::BusError::Overlap
            )
        );
        assert_eq!(
            format!(
                "{}",
                Error::CreateMmioDevice(io::Error::from_raw_os_error(0))
            ),
            format!(
                "failed to create mmio device: {}",
                io::Error::from_raw_os_error(0)
            )
        );
        assert_eq!(
            format!("{}", Error::IrqsExhausted),
            "no more IRQs are available"
        );
        assert_eq!(
            format!(
                "{}",
                Error::RegisterIoEvent(io::Error::from_raw_os_error(0))
            ),
            format!(
                "failed to register IO event: {}",
                io::Error::from_raw_os_error(0)
            )
        );
        assert_eq!(
            format!("{}", Error::RegisterIrqFd(io::Error::from_raw_os_error(0))),
            format!(
                "failed to register irqfd: {}",
                io::Error::from_raw_os_error(0)
            )
        );
    }

    #[test]
    fn test_update_drive() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem = GuestMemory::new(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem, 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy_box = Box::new(DummyDevice { dummy: 0 });
        let vmm = create_vmm_object();

        if let Ok(addr) =
            device_manager.register_virtio_device(vmm.vm.get_fd(), dummy_box, &mut cmdline, "foo")
        {
            assert!(device_manager.update_drive(addr, 1_048_576).is_ok());
        }
        assert!(device_manager.update_drive(0xbeef, 1_048_576).is_err());
    }

    #[test]
    fn test_device_info() {
        let start_addr1 = GuestAddress(0x0);
        let start_addr2 = GuestAddress(0x1000);
        let guest_mem = GuestMemory::new(&[(start_addr1, 0x1000), (start_addr2, 0x1000)]).unwrap();
        let mut device_manager =
            MMIODeviceManager::new(guest_mem, 0xd000_0000, (arch::IRQ_BASE, arch::IRQ_MAX));
        let mut cmdline = kernel_cmdline::Cmdline::new(4096);
        let dummy_box = Box::new(DummyDevice { dummy: 0 });
        let vmm = create_vmm_object();

        let id = String::from("foo");
        if let Ok(addr) =
            device_manager.register_virtio_device(vmm.vm.get_fd(), dummy_box, &mut cmdline, &id)
        {
            assert_eq!(Some(&addr), device_manager.get_address(&id));
            assert_eq!(addr, device_manager.id_to_dev_info.get(&id).unwrap().addr);
            assert_eq!(
                arch::IRQ_BASE,
                device_manager.id_to_dev_info.get(&id).unwrap().irq
            );
            assert_eq!(
                DeviceType::Virtio,
                device_manager.id_to_dev_info.get(&id).unwrap().type_
            );
        }
        let id = "bar";
        assert_eq!(None, device_manager.get_address(&id));
    }
}
