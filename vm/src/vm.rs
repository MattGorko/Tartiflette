use crate::bits::BitField;
use crate::memory::{Mapping, MemoryError, PagePermissions, VirtualMemory, PAGE_SIZE};
use crate::snapshot::{SnapshotError, SnapshotInfo, SnapshotRegisters};
use crate::x64::{
    ExceptionFrame, ExceptionType, IdtEntry, IdtEntryBuilder, IdtEntryType, PrivilegeLevel, Tss,
    TssEntry,
};

use kvm_bindings::{
    kvm_clear_dirty_log, kvm_enable_cap, kvm_guest_debug, kvm_msr_entry, kvm_regs, kvm_segment,
    kvm_sregs, kvm_userspace_memory_region, Msrs, KVMIO, KVM_API_VERSION,
    KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2, KVM_DIRTY_LOG_MANUAL_PROTECT_ENABLE, KVM_GUESTDBG_ENABLE,
    KVM_GUESTDBG_USE_SW_BP, KVM_MEM_LOG_DIRTY_PAGES, KVM_SYNC_X86_REGS, KVM_SYNC_X86_SREGS,
};
use kvm_ioctls::{Cap, Kvm, KvmRunWrapper, VcpuExit, VcpuFd, VmFd};
use nix::errno::Errno;

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use vmm_sys_util::ioctl;

type Result<T> = std::result::Result<T, VmError>;

ioctl_iowr_nr!(KVM_CLEAR_DIRTY_LOG, KVMIO, 0xC0, kvm_clear_dirty_log);
ioctl_io_nr!(KVM_CHECK_EXTENSION, KVMIO, 0x03);

/// FS base MSR number
const IA32_FS_BASE: u32 = 0xC0000100;
/// GS base MSR numebr
const IA32_GS_BASE: u32 = 0xC0000101;

/// Vm manipulation error
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VmError {
    /// Error during a memory access
    MemoryError(MemoryError),
    /// Error during snapshot loading
    SnapshotError(SnapshotError),
    /// Hypervisor error
    HvError(&'static str),
}

impl From<MemoryError> for VmError {
    fn from(err: MemoryError) -> VmError {
        VmError::MemoryError(err)
    }
}

impl From<std::io::Error> for VmError {
    fn from(err: std::io::Error) -> VmError {
        VmError::SnapshotError(SnapshotError::IoError(err.to_string()))
    }
}

impl From<SnapshotError> for VmError {
    fn from(err: SnapshotError) -> VmError {
        VmError::SnapshotError(err)
    }
}

/// List of available registers
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Register {
    /// RAX
    Rax,
    /// RBX
    Rbx,
    /// RCX
    Rcx,
    /// RDX
    Rdx,
    /// RSI
    Rsi,
    /// RDI
    Rdi,
    /// RSP
    Rsp,
    /// RBP
    Rbp,
    /// R8
    R8,
    /// R9
    R9,
    /// R10
    R10,
    /// R11
    R11,
    /// R12
    R12,
    /// R13
    R13,
    /// R14
    R14,
    /// R15
    R15,
    /// RIP
    Rip,
    /// RFLAGS
    Rflags,
    /// FS BASE
    FsBase,
    /// GS BASE
    GsBase,
}

/// Additional details behind a PageFault exception
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct PageFaultDetail {
    /// Page fault status code (from the exception frame)
    pub status: u32,
    /// Address of the access which caused the fault
    pub address: u64,
}

impl PageFaultDetail {
    /// Returns true if the faulty access was made to unmapped memory.
    #[inline]
    pub fn unmapped(&self) -> bool {
        self.status.is_bit_set(0)
    }

    /// Returns true if the faulty access was a read.
    #[inline]
    pub fn read(&self) -> bool {
        self.status.is_bit_set(1)
    }

    /// Returns true if the faulty access was a write.
    #[inline]
    pub fn write(&self) -> bool {
        !self.read()
    }

    /// Returns true if the faulty access was an instruction fetch.
    #[inline]
    pub fn instruction_fetch(&self) -> bool {
        self.status.is_bit_set(15)
    }
}

/// Vm exit reason
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum VmExit {
    /// Vm stopped on a halt instruction
    Hlt,
    /// Vm stopped on a breakpoint instruction or singlestep
    Breakpoint,
    /// Vm interrupted by the hypervisor
    Interrupted,
    /// Vm stopped on an invalid instruction
    InvalidInstruction,
    /// Vm stopped on a page fault
    PageFault(PageFaultDetail),
    /// Vm stopped on an unhandled exception
    Exception(u64),
    /// Vm stopped on a syscall instruction
    Syscall,
    /// Vmexit unhandled by tartiflette
    Unhandled,
}

/// Tartiflette vm state
pub struct Vm {
    /// Kvm device file descriptor
    _kvm: Kvm,
    /// Kvm vm file descriptor
    kvm_vm: VmFd,
    /// Kvm vm vcpu file descriptor
    kvm_vcpu: VcpuFd,
    /// Kvm vcpu run
    kvm_vcpu_run: KvmRunWrapper,
    /// Local copy of kvm registers
    registers: kvm_regs,
    /// Local copy of kvm special registers
    special_registers: kvm_sregs,
    /// fs_base register
    fs_base: u64,
    /// gs_base register
    gs_base: u64,
    /// Starting address of the hypercall region
    hypercall_page: u64,
    /// Vm Memory
    pub memory: VirtualMemory,
}

impl Vm {
    /// Creates a new `Vm` instance with a given memory size
    /// (the size will be aligned to the nearest page multiple).
    pub fn new(memory_size: usize) -> Result<Vm> {
        // Create minimal vm
        let mut vm = Vm::setup_barebones(memory_size)?;

        // Setup special registers
        vm.setup_registers()?;

        // Setup exception handling
        vm.setup_exception_handling()?;

        // Flush registers
        vm.flush_registers()?;

        Ok(vm)
    }

    /// Sets up a minimal working vm environnement.
    /// (kvm init + memory + sregs)
    fn setup_barebones(memory_size: usize) -> Result<Vm> {
        // 1 - Allocate the memory
        let vm_memory = VirtualMemory::new(memory_size)?;

        // 2 - Open the kvm device and check some stuff
        let kvm_fd = Kvm::new().map_err(|_| VmError::HvError("Could not open kvm device"))?;

        // Check the kvm api version
        if kvm_fd.get_api_version() as u32 != KVM_API_VERSION {
            return Err(VmError::HvError("Wrong KVM api version"));
        }

        // Check the `SyncRegs` extension
        if !kvm_fd.check_extension(Cap::SyncRegs) {
            return Err(VmError::HvError("SyncRegs capability not present"));
        }

        // Check the KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2 extension
        let ret = unsafe {
            ioctl::ioctl_with_val(
                &kvm_fd,
                KVM_CHECK_EXTENSION(),
                KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2 as u64,
            )
        };
        if ret <= 0 {
            return Err(VmError::HvError(
                "Manual dirty log protect capability not present",
            ));
        }

        // 3 - Ask kvm to create a vm
        let vm_fd = kvm_fd
            .create_vm()
            .map_err(|_| VmError::HvError("Could not create vm fd"))?;

        // Enable the KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2 capability
        let mut cap = kvm_enable_cap::default();
        cap.cap = KVM_CAP_MANUAL_DIRTY_LOG_PROTECT2;
        cap.args[0] = KVM_DIRTY_LOG_MANUAL_PROTECT_ENABLE as u64;
        vm_fd
            .enable_cap(&cap)
            .expect("Could not enable KVM_DIRTY_LOG_MANUAL_PROTECT");

        // 4 - Ask kvm to create a new vcpu for our vm
        let vcpu_fd = vm_fd
            .create_vcpu(0)
            .map_err(|_| VmError::HvError("Could not create vm vcpu"))?;

        // 5 - Map the VCPU kvm run memory region
        let vcpu_mmap_size = kvm_fd
            .get_vcpu_mmap_size()
            .map_err(|_| VmError::HvError("Could not get vcpu mmap size"))?;
        let vcpu_run = KvmRunWrapper::mmap_from_fd(&vcpu_fd, vcpu_mmap_size)
            .map_err(|_| VmError::HvError("Could not get wrapper arround vcpu"))?;

        // 6 - Setup guest memory
        unsafe {
            let region = kvm_userspace_memory_region {
                slot: 0,
                guest_phys_addr: 0,
                memory_size: vm_memory.host_memory_size() as u64,
                userspace_addr: vm_memory.host_address(),
                flags: KVM_MEM_LOG_DIRTY_PAGES,
            };
            vm_fd
                .set_user_memory_region(region)
                .map_err(|_| VmError::HvError("Could not set memory region for guest"))?
        }

        // Get registers
        let regs = vcpu_fd
            .get_regs()
            .map_err(|_| VmError::HvError("Could not get general registers"))?;
        // Get special registers
        let sregs = vcpu_fd
            .get_sregs()
            .map_err(|_| VmError::HvError("Could not get special registers"))?;

        // Construct the new `Vm` object
        Ok(Vm {
            _kvm: kvm_fd,
            kvm_vm: vm_fd,
            kvm_vcpu: vcpu_fd,
            kvm_vcpu_run: vcpu_run,
            registers: regs,
            special_registers: sregs,
            memory: vm_memory,
            hypercall_page: 0,
            fs_base: 0,
            gs_base: 0,
        })
    }

    /// Configures the Vm special registers
    fn setup_registers(&mut self) -> Result<()> {
        // Initialize system registers
        const CR0_PG: u64 = 1 << 31;
        const CR0_PE: u64 = 1 << 0;
        const CR0_ET: u64 = 1 << 4;
        const CR0_WP: u64 = 1 << 16;

        // TODO: Check CPUID before setting the flags or get the crX regs from a snapshot
        const CR4_PAE: u64 = 1 << 5;
        const CR4_OSXSAVE: u64 = 1 << 18;
        const CR4_OSFXSR: u64 = 1 << 9;
        const IA32_EFER_LME: u64 = 1 << 8;
        const IA32_EFER_LMA: u64 = 1 << 10;
        const IA32_EFER_NXE: u64 = 1 << 11;

        // Set the 64 bits code segment
        let mut seg = kvm_segment {
            base: 0,
            limit: 0,
            selector: 1 << 3, // Index 1, GDT, RPL = 0
            present: 1,
            type_: 11, /* Code: execute, read, accessed */
            dpl: 0,
            db: 0,
            s: 1, /* Code/data */
            l: 1,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };
        self.special_registers.cs = seg;

        // seg.selector = 0;
        seg.type_ = 3;

        // Set the others 64 bits segments
        self.special_registers.ds = seg;
        self.special_registers.es = seg;
        self.special_registers.fs = seg;
        self.special_registers.gs = seg;
        self.special_registers.ss = seg;

        // Paging enable and paging
        self.special_registers.cr0 = CR0_PE | CR0_PG | CR0_ET | CR0_WP;
        // Physical address extension (necessary for x64)
        self.special_registers.cr4 = CR4_PAE | CR4_OSXSAVE | CR4_OSFXSR;
        // Sets the page table root address
        self.special_registers.cr3 = self.memory.page_directory() as u64;
        // Sets x64 mode enabled (LME), active (LMA), executable disable bit support (NXE), syscall
        // support (SCE)
        self.special_registers.efer = IA32_EFER_LME | IA32_EFER_LMA | IA32_EFER_NXE;

        // Set the tss address
        self.kvm_vm
            .set_tss_address(0xfffb_d000)
            .map_err(|_| VmError::HvError("Could not set tss address"))?;

        // Enable vm exit on software breakpoints
        let debug_struct = kvm_guest_debug {
            control: KVM_GUESTDBG_ENABLE | KVM_GUESTDBG_USE_SW_BP,
            pad: 0,
            arch: Default::default(),
        };
        self.kvm_vcpu
            .set_guest_debug(&debug_struct)
            .map_err(|_| VmError::HvError("Could not set debug registers"))?;

        Ok(())
    }

    /// Setups the necessary pieces for handling interrupts (TSS, TSS Stack, GDT slots, IDT)
    fn setup_exception_handling(&mut self) -> Result<()> {
        // Defines usefull regions
        const IDT_ADDRESS: u64 = 0xffff_ffff_ff00_0000;
        const IDT_HANDLERS: u64 = IDT_ADDRESS + PAGE_SIZE as u64;
        const GDT_ADDRESS: u64 = IDT_ADDRESS + (PAGE_SIZE * 2) as u64;
        const TSS_ADDRESS: u64 = IDT_ADDRESS + (PAGE_SIZE * 3) as u64;
        const STACK_ADDRESS: u64 = IDT_ADDRESS + (PAGE_SIZE * 4) as u64;

        // A stack size of 4KB should be enough for simply handling interrupts
        const STACK_SIZE: usize = PAGE_SIZE;

        // Setting up the GDT
        self.memory.mmap(
            GDT_ADDRESS,
            PAGE_SIZE,
            PagePermissions::READ | PagePermissions::WRITE,
        )?;

        // Setting up the null segment
        self.memory.write_val(GDT_ADDRESS, 0u64)?;
        // Setting up the 64 bits code segment
        self.memory
            .write_val(GDT_ADDRESS + 8, 0x00209a0000000000u64)?;
        // Setting up the TSS entry
        self.memory.write_val(
            GDT_ADDRESS + 16,
            TssEntry::new(TSS_ADDRESS, PrivilegeLevel::Ring0),
        )?;

        // Set the sepecial registers to reference the GDT
        self.special_registers.gdt.base = GDT_ADDRESS;
        self.special_registers.gdt.limit = (8 * 3) - 1;

        // Setting up the TSS
        self.memory
            .mmap(TSS_ADDRESS, PAGE_SIZE, PagePermissions::READ)?;

        // Create the TSS with an IST alternative stack at index 1
        let mut tss = Tss::new();
        tss.set_ist(1, STACK_ADDRESS + (STACK_SIZE - 0x100) as u64);
        // Write the structure in memory
        self.memory.write_val(TSS_ADDRESS, tss)?;

        // Set the tr register to the TSS
        self.special_registers.tr = kvm_segment {
            base: TSS_ADDRESS,
            limit: (core::mem::size_of::<Tss>() - 1) as u32,
            selector: 2 << 3, // Index 2, GDT, RPL = 0
            present: 1,
            type_: 11,
            dpl: 0,
            db: 0,
            s: 0,
            l: 1,
            g: 0,
            avl: 0,
            unusable: 0,
            padding: 0,
        };

        // Setting up exception handlers
        self.memory.mmap(
            IDT_HANDLERS,
            PAGE_SIZE,
            PagePermissions::READ | PagePermissions::EXECUTE,
        )?;
        self.hypercall_page = IDT_HANDLERS;

        // Loop through IDT handlers
        for i in 0..32 {
            let handler_code: &[u8] = &[
                0x6a, i as u8, // push <exception index>
                0xf4,    // hlt -> our hypercall
            ];

            self.memory.write(IDT_HANDLERS + (i * 32), handler_code)?;
        }

        // Setting up the IDT
        self.memory
            .mmap(IDT_ADDRESS, PAGE_SIZE, PagePermissions::READ)?;

        let mut entries = [IdtEntry::new(); 32];
        let entries_size = entries.len() * std::mem::size_of::<IdtEntry>();

        // Loop through IDT entries
        for i in 0..32 {
            entries[i] = IdtEntryBuilder::new()
                .base(IDT_HANDLERS + (i * 32) as u64)
                .dpl(PrivilegeLevel::Ring0)
                .segment_selector(1, PrivilegeLevel::Ring0)
                .gate_type(IdtEntryType::Trap)
                .ist(1)
                .collect();
        }
        self.memory.write_val(IDT_ADDRESS, entries)?;

        // Set the sepecial registers to reference the IDT
        self.special_registers.idt.base = IDT_ADDRESS;
        self.special_registers.idt.limit = (entries_size - 1) as u16;

        // Setting up the alternativ stack by allocating it for exception handling
        self.memory.mmap(
            STACK_ADDRESS,
            STACK_SIZE,
            PagePermissions::READ | PagePermissions::WRITE,
        )?;

        Ok(())
    }

    /// Gets a register from the vm state
    #[inline]
    pub fn get_reg(&self, regid: Register) -> u64 {
        match regid {
            Register::Rax => self.registers.rax,
            Register::Rbx => self.registers.rbx,
            Register::Rcx => self.registers.rcx,
            Register::Rdx => self.registers.rdx,
            Register::Rsi => self.registers.rsi,
            Register::Rdi => self.registers.rdi,
            Register::Rsp => self.registers.rsp,
            Register::Rbp => self.registers.rbp,
            Register::R8 => self.registers.r8,
            Register::R9 => self.registers.r9,
            Register::R10 => self.registers.r10,
            Register::R11 => self.registers.r11,
            Register::R12 => self.registers.r12,
            Register::R13 => self.registers.r13,
            Register::R14 => self.registers.r14,
            Register::R15 => self.registers.r15,
            Register::Rip => self.registers.rip,
            Register::Rflags => self.registers.rflags,
            Register::FsBase => self.fs_base,
            Register::GsBase => self.gs_base,
        }
    }

    /// Sets a register in the vm state
    #[inline]
    pub fn set_reg(&mut self, regid: Register, regval: u64) {
        match regid {
            Register::Rax => self.registers.rax = regval,
            Register::Rbx => self.registers.rbx = regval,
            Register::Rcx => self.registers.rcx = regval,
            Register::Rdx => self.registers.rdx = regval,
            Register::Rsi => self.registers.rsi = regval,
            Register::Rdi => self.registers.rdi = regval,
            Register::Rsp => self.registers.rsp = regval,
            Register::Rbp => self.registers.rbp = regval,
            Register::R8 => self.registers.r8 = regval,
            Register::R9 => self.registers.r9 = regval,
            Register::R10 => self.registers.r10 = regval,
            Register::R11 => self.registers.r11 = regval,
            Register::R12 => self.registers.r12 = regval,
            Register::R13 => self.registers.r13 = regval,
            Register::R14 => self.registers.r14 = regval,
            Register::R15 => self.registers.r15 = regval,
            Register::Rip => self.registers.rip = regval,
            Register::Rflags => self.registers.rflags = regval,
            Register::FsBase => self.fs_base = regval,
            Register::GsBase => self.gs_base = regval,
        }
    }

    /// Maps memory with given permissions in the vm address space
    #[inline]
    pub fn mmap(&mut self, vaddr: u64, size: usize, perms: PagePermissions) -> Result<()> {
        self.memory
            .mmap(vaddr, size, perms)
            .map_err(VmError::MemoryError)
    }

    /// Writes given data to the vm memory
    #[inline]
    pub fn write(&mut self, vaddr: u64, data: &[u8]) -> Result<()> {
        self.memory.write(vaddr, data).map_err(VmError::MemoryError)
    }

    /// Writes a value to the vm memory
    #[inline]
    pub fn write_value<T>(&mut self, address: u64, val: T) -> Result<()> {
        self.memory
            .write_val::<T>(address, val)
            .map_err(VmError::MemoryError)
    }

    /// Reads data from the given vm memory
    #[inline]
    pub fn read(&self, vaddr: u64, data: &mut [u8]) -> Result<()> {
        self.memory.read(vaddr, data).map_err(VmError::MemoryError)
    }

    /// Returns an iterator over all mappings
    #[inline]
    pub fn mappings(&self) -> impl Iterator<Item = Mapping> + '_ {
        self.memory.mappings()
    }

    /// Returns an iterator over all dirty mappings
    #[inline]
    pub fn dirty_mappings(&self) -> impl Iterator<Item = Mapping> + '_ {
        self.mappings().filter(|m| m.dirty)
    }

    /// Clear dirty mappings status
    #[inline]
    pub fn clear_dirty_mappings(&mut self) {
        for (_, pte) in self.memory.raw_pages_mut() {
            pte.set_dirty(false);
        }
    }

    fn flush_registers(&mut self) -> Result<()> {
        // The second bit of rflags must always be set.
        self.registers.rflags |= 1 << 1;

        // Set registers and special registers
        self.kvm_vcpu
            .set_regs(&self.registers)
            .map_err(|_| VmError::HvError("Could not commit registers"))?;
        self.kvm_vcpu
            .set_sregs(&self.special_registers)
            .map_err(|_| VmError::HvError("Could not commit special registers"))?;

        // Set gs_base and fs_base through msrs
        let msrs = Msrs::from_entries(&[
            kvm_msr_entry {
                index: IA32_FS_BASE,
                data: self.fs_base,
                ..Default::default()
            },
            kvm_msr_entry {
                index: IA32_GS_BASE,
                data: self.gs_base,
                ..Default::default()
            },
        ])
        .unwrap();
        self.kvm_vcpu
            .set_msrs(&msrs)
            .map_err(|_| VmError::HvError("Could not commit fsbase and gsbase"))?;

        // Get registers and special registers
        self.registers = self
            .kvm_vcpu
            .get_regs()
            .map_err(|_| VmError::HvError("Could not get special registers"))?;
        self.special_registers = self
            .kvm_vcpu
            .get_sregs()
            .map_err(|_| VmError::HvError("Could not get general registers"))?;

        // Update kvm vcpu run region
        self.kvm_vcpu_run.as_mut_ref().s.regs.regs = self.registers;
        self.kvm_vcpu_run.as_mut_ref().s.regs.sregs = self.special_registers;
        self.kvm_vcpu_run.as_mut_ref().kvm_dirty_regs = 0;

        Ok(())
    }

    /// Commit local copy of registers to kvm
    #[inline]
    fn commit_registers(&mut self) -> Result<()> {
        // The second bit of rflags must always be set.
        self.registers.rflags |= 1 << 1;

        self.kvm_vcpu_run.as_mut_ref().s.regs.regs = self.registers;
        self.kvm_vcpu_run.as_mut_ref().s.regs.sregs = self.special_registers;
        self.kvm_vcpu_run.as_mut_ref().kvm_dirty_regs |=
            KVM_SYNC_X86_SREGS as u64 | KVM_SYNC_X86_REGS as u64;

        // gs_base and fs_base need to go through msrs
        let msrs = Msrs::from_entries(&[
            kvm_msr_entry {
                index: IA32_FS_BASE,
                data: self.fs_base,
                ..Default::default()
            },
            kvm_msr_entry {
                index: IA32_GS_BASE,
                data: self.gs_base,
                ..Default::default()
            },
        ])
        .unwrap();

        self.kvm_vcpu
            .set_msrs(&msrs)
            .map_err(|_| VmError::HvError("Could not commit fsbase and gsbase"))?;

        Ok(())
    }

    /// Run the `Vm` instance until the first `Vm` that cannot be
    /// handled directly
    pub fn run(&mut self) -> Result<VmExit> {
        let result = loop {
            // Commit potential modification done on registers
            self.commit_registers()?;

            // Set the valid synchronised registers
            self.kvm_vcpu_run.as_mut_ref().kvm_valid_regs |=
                KVM_SYNC_X86_REGS as u64 | KVM_SYNC_X86_SREGS as u64;

            // Ask kvm to run the vm's vcpu
            let exit = self.kvm_vcpu.run();

            // Pull registers and special registers
            unsafe {
                self.registers = self.kvm_vcpu_run.as_mut_ref().s.regs.regs;
                self.special_registers = self.kvm_vcpu_run.as_mut_ref().s.regs.sregs;
            }

            // Pull fs_base and gs_base
            let mut msrs = Msrs::from_entries(&[
                kvm_msr_entry {
                    index: IA32_FS_BASE,
                    ..Default::default()
                },
                kvm_msr_entry {
                    index: IA32_GS_BASE,
                    ..Default::default()
                },
            ])
            .unwrap();

            let count = self
                .kvm_vcpu
                .get_msrs(&mut msrs)
                .map_err(|_| VmError::HvError("Could not read fs_base and gs_base"))?;
            assert_eq!(count, 2, "Invalid number of msrs returned");

            let msrs_res = msrs.as_slice();
            self.fs_base = msrs_res[0].data;
            self.gs_base = msrs_res[1].data;

            // Handle possible interrupts (timeout)
            if let Err(err) = exit {
                match Errno::from_i32(err.errno()) {
                    Errno::EINTR | Errno::EAGAIN => break VmExit::Interrupted,
                    _ => return Err(VmError::HvError("Unexpected errno in KVM_RUN")),
                }
            }

            match exit.unwrap() {
                VcpuExit::Debug(_) => {
                    break VmExit::Breakpoint;
                }
                VcpuExit::Hlt => {
                    // If code is outside of hypercall region, forward the hlt
                    if (self.registers.rip < self.hypercall_page)
                        || (self.registers.rip >= self.hypercall_page + PAGE_SIZE as u64)
                    {
                        break VmExit::Hlt;
                    }

                    // If we are within the hypercall region, handle the
                    // exception forwarding.
                    let exception_code: u64 = self.memory.read_val(self.registers.rsp)?;

                    let error_code: Option<u64> = match ExceptionType::from(exception_code) {
                        ExceptionType::DoubleFault
                        | ExceptionType::InvalidTSS
                        | ExceptionType::SegmentNotPresent
                        | ExceptionType::StackFault
                        | ExceptionType::GeneralProtection
                        | ExceptionType::PageFault
                        | ExceptionType::AlignmentCheck
                        | ExceptionType::ControlProtection => {
                            Some(self.memory.read_val(self.registers.rsp + 8)?)
                        }
                        _ => None,
                    };

                    let exception_frame: ExceptionFrame = if error_code.is_some() {
                        self.memory.read_val(self.registers.rsp + 16)?
                    } else {
                        self.memory.read_val(self.registers.rsp + 8)?
                    };

                    // Reset register context to before exception
                    self.registers.rsp = exception_frame.rsp;
                    self.registers.rip = exception_frame.rip;

                    match ExceptionType::from(exception_code) {
                        ExceptionType::PageFault => {
                            break VmExit::PageFault(PageFaultDetail {
                                status: error_code.unwrap() as u32,
                                address: self.special_registers.cr2,
                            });
                        }
                        ExceptionType::InvalidOpcode => {
                            // As IA32_EFER.SCE is not enabled, a syscall instruction will trigger
                            // a #UD exception. We cannot enable the SCE bit in EFER as it would
                            // require us to setup the whole syscall machinery as well as the LSTAR
                            // register.
                            // To give the opportunity to the Vm user to emulate the syscall, we try
                            // to detect the instruction bytes, set the rip to after the syscall
                            // and return with a special `Syscall` VmExit.
                            let mut code_bytes: [u8; 2] = [0; 2];

                            if self
                                .memory
                                .read(self.registers.rip, &mut code_bytes)
                                .is_ok()
                            {
                                //  0f 05 -> syscall
                                if code_bytes == [0x0f, 0x05] {
                                    // We advance rip by two bytes to move over the syscall
                                    // instruction.
                                    self.registers.rip += 2;
                                    break VmExit::Syscall;
                                }
                            }

                            break VmExit::InvalidInstruction;
                        }
                        _ => break VmExit::Exception(exception_code),
                    }
                }
                _ => break VmExit::Unhandled,
            }
        };

        Ok(result)
    }

    // Set `Vm` registers from a `SnapshotRegisters` instance
    #[inline]
    pub fn set_regs_snapshot(&mut self, regs: &SnapshotRegisters) {
        self.set_reg(Register::Rax, regs.rax);
        self.set_reg(Register::Rbx, regs.rbx);
        self.set_reg(Register::Rcx, regs.rcx);
        self.set_reg(Register::Rdx, regs.rdx);
        self.set_reg(Register::Rsi, regs.rsi);
        self.set_reg(Register::Rdi, regs.rdi);
        self.set_reg(Register::Rsp, regs.rsp);
        self.set_reg(Register::Rbp, regs.rbp);
        self.set_reg(Register::R8, regs.r8);
        self.set_reg(Register::R9, regs.r9);
        self.set_reg(Register::R10, regs.r10);
        self.set_reg(Register::R11, regs.r11);
        self.set_reg(Register::R12, regs.r12);
        self.set_reg(Register::R13, regs.r13);
        self.set_reg(Register::R14, regs.r14);
        self.set_reg(Register::R15, regs.r15);
        self.set_reg(Register::Rip, regs.rip);
        self.set_reg(Register::Rflags, regs.rflags);
        self.set_reg(Register::FsBase, regs.fs_base);
        self.set_reg(Register::GsBase, regs.gs_base);
    }

    /// Loads a vm state from snapshot files
    pub fn from_snapshot<T: AsRef<Path>>(
        snapshot_info: T,
        memory_dump: T,
        memory_size: usize,
    ) -> Result<Vm> {
        // Create a new VN instance
        let mut vm = Vm::new(memory_size)?;

        // Get the snapshot information
        let info = SnapshotInfo::from_file(snapshot_info)?;

        // Loading the mappings
        let mut dump = File::open(memory_dump)?;
        let mut buf: [u8; PAGE_SIZE] = [0; PAGE_SIZE];

        // Loop through mapping
        for mapping in info.mappings {
            assert!(mapping.start < mapping.end, "mapping.start > mapping.end");

            // Create the mapping
            let mapping_size = (mapping.end - mapping.start) as usize;
            vm.mmap(mapping.start, mapping_size, mapping.permissions)?;

            // TODO: Implement more efficient copy to memory
            // Loop through each page of the mapping and copy it
            for off in (0..mapping_size).step_by(PAGE_SIZE) {
                dump.seek(SeekFrom::Start(mapping.physical_offset + off as u64))?;
                dump.read(&mut buf)?;
                vm.write(mapping.start + off as u64, &buf)?;
            }
        }

        // Load all the registers
        vm.set_regs_snapshot(&info.registers);
        vm.flush_registers()?;

        Ok(vm)
    }

    /// Reset the `Vm` state from an other one
    pub fn reset(&mut self, other: &Vm) {
        // Reset registers
        self.registers = other.registers;
        self.special_registers = other.special_registers;
        self.fs_base = other.fs_base;
        self.gs_base = other.gs_base;

        // Reset memory state
        // Here we prefer aborting as if you are resetting a vm with a completely different one you
        // are doing something extremely wrong.
        assert_eq!(
            self.memory.host_memory_size(),
            other.memory.host_memory_size(),
            "Vm memory mismatch"
        );

        // Get the dirty log from kvm
        let dirty_log = self
            .kvm_vm
            .get_dirty_log(0, self.memory.host_memory_size())
            .expect("Could not get dirty log for current vm");

        // Loop through each dirty page and reset it
        for (bm_index, bm_entry) in dirty_log.iter().enumerate() {
            let mut bm = *bm_entry;

            while bm != 0 {
                // Get next frame dirtied
                let i = bm.trailing_zeros() as usize;
                let pa = (bm_index * 64 + i) * PAGE_SIZE;

                // Get raw mutable slice to the pmem to restore
                let mut page_data = self
                    .memory
                    .pmem
                    .raw_slice_mut(pa, PAGE_SIZE)
                    .expect("Could not restore page in dirty vm");

                // Read original data to the slice
                other
                    .memory
                    .pmem
                    .read(pa, &mut page_data)
                    .expect("Could not read physical memory from source vm");

                // Go tp the next bit
                bm &= bm - 1;
            }
        }

        // Define the dirty log clear structure
        let dirty_log = kvm_bindings::kvm_clear_dirty_log {
            slot: 0,
            num_pages: (self.memory.host_memory_size() / PAGE_SIZE) as u32,
            first_page: 0,
            __bindgen_anon_1: kvm_bindings::kvm_clear_dirty_log__bindgen_ty_1 {
                dirty_bitmap: dirty_log.as_ptr() as *mut core::ffi::c_void,
            },
        };

        // Clear dirty log
        let ret = unsafe { ioctl::ioctl_with_ref(&self.kvm_vm, KVM_CLEAR_DIRTY_LOG(), &dirty_log) };
        if ret != 0 {
            panic!("Failed to clean dirty log");
        }
    }
}

impl Clone for Vm {
    fn clone(&self) -> Self {
        let mut vm =
            Vm::new(self.memory.host_memory_size()).expect("Could not create vm for clone");

        // Copy registers
        vm.registers = self.registers;
        vm.special_registers = self.special_registers;
        vm.fs_base = self.fs_base;
        vm.gs_base = self.gs_base;

        // Copy memory
        let orig_mem = self
            .memory
            .pmem
            .raw_slice(0, self.memory.host_memory_size())
            .expect("Could not get original physical memory");
        vm.memory
            .pmem
            .write(0, &orig_mem)
            .expect("Could not set actual memory to original");

        vm
    }
}

#[cfg(test)]
mod tests {
    use super::{Register, Result, Vm, VmExit};
    use crate::memory::{PagePermissions, PAGE_SIZE};

    #[test]
    /// Runs a simple piece of code until completion
    fn test_simple_exec() -> Result<()> {
        let mut vm = Vm::new(512 * PAGE_SIZE)?;

        // Simple shellcode
        let shellcode: &[u8] = &[
            0x48, 0x01, 0xc2, // add rdx, rax
            0xcc, // breakpoint
        ];

        // Mapping the code
        vm.mmap(0x1337000, PAGE_SIZE, PagePermissions::EXECUTE)?;
        vm.write(0x1337000, shellcode)?;

        // Set registers to known values
        vm.set_reg(Register::Rax, 0x1000);
        vm.set_reg(Register::Rdx, 0x337);

        // Execute from beginning of shellcode
        vm.set_reg(Register::Rip, 0x1337000);

        let vmexit = vm.run()?;

        assert_eq!(vmexit, VmExit::Breakpoint);
        assert_eq!(vm.get_reg(Register::Rip), 0x1337003);

        Ok(())
    }

    #[test]
    /// Tests the collection and clearing of dirty pages
    fn test_dirty_status() -> Result<()> {
        let mut vm = Vm::new(512 * PAGE_SIZE)?;

        // Simple shellcode
        let shellcode: &[u8] = &[
            0x48, 0x89, 0x10, // mov [rax], rdx
            0xcc, // int3
        ];

        // Mapping the code
        vm.mmap(0x1337000, PAGE_SIZE, PagePermissions::EXECUTE)?;
        vm.write(0x1337000, shellcode)?;

        // Mapping the target page of the write
        vm.mmap(
            0xdeadb000,
            PAGE_SIZE,
            PagePermissions::READ | PagePermissions::WRITE,
        )?;

        // Set registers to known values
        vm.set_reg(Register::Rax, 0xdeadbeef);
        vm.set_reg(Register::Rdx, 0x42424242);

        // Execute from beginning of shellcode
        vm.set_reg(Register::Rip, 0x1337000);

        let vmexit = vm.run()?;

        // Sanity check
        assert_eq!(vmexit, VmExit::Breakpoint);
        assert_eq!(vm.get_reg(Register::Rip), 0x1337003);

        // Check that the target page was dirtied
        assert!(vm.dirty_mappings().any(|m| m.address == 0xdeadb000));

        // Reset the pages dirty status
        vm.clear_dirty_mappings();

        // Check again the dirty pages
        assert!(vm.dirty_mappings().count() == 0);

        Ok(())
    }

    #[test]
    /// Runs a simple piece of code until completion
    fn test_simple_syscall() -> Result<()> {
        let mut vm = Vm::new(512 * PAGE_SIZE)?;

        // The syscall in the shellcode will add rax and rdx together
        let shellcode: &[u8] = &[
            0x0f, 0x05, // syscall
            0xcc, // breakpoint
        ];

        // Mapping the code
        vm.mmap(0x1337000, PAGE_SIZE, PagePermissions::EXECUTE)?;
        vm.write(0x1337000, shellcode)?;

        // Set registers to known values
        vm.set_reg(Register::Rax, 0x1000);
        vm.set_reg(Register::Rdx, 0x337);

        // Execute from beginning of shellcode
        vm.set_reg(Register::Rip, 0x1337000);

        let vmexit = vm.run()?;

        assert_eq!(vmexit, VmExit::Syscall);

        // Emulated syscall doing rax = rax + rdx
        vm.set_reg(
            Register::Rax,
            vm.get_reg(Register::Rax) + vm.get_reg(Register::Rdx),
        );

        let vmexit_end = vm.run()?;

        assert_eq!(vmexit_end, VmExit::Breakpoint);
        assert_eq!(vm.get_reg(Register::Rip), 0x1337002);
        assert_eq!(vm.get_reg(Register::Rax), 0x1337);

        Ok(())
    }
}
