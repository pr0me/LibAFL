/*!
The frida address sanitizer runtime provides address sanitization.
When executing in `ASAN`, each memory access will get checked, using frida stalker under the hood.
The runtime can report memory errors that occurred during execution,
even if the target would not have crashed under normal conditions.
this helps finding mem errors early.
*/

use frida_gum::NativePointer;
use hashbrown::HashMap;
use libafl::{
    bolts::{
        os::{find_mapping_for_address, find_mapping_for_path, walk_self_maps},
        ownedref::OwnedPtr,
        tuples::Named,
    },
    corpus::Testcase,
    events::EventFirer,
    executors::{CustomExitKind, ExitKind, HasExecHooks},
    feedbacks::Feedback,
    inputs::{HasTargetBytes, Input},
    observers::{Observer, ObserversTuple},
    state::HasMetadata,
    Error, SerdeAny,
};
use nix::{libc::{memmove, memset}, sys::mman::{MapFlags, ProtFlags, mmap}};

use backtrace::Backtrace;
use capstone::{
    arch::{arm64::Arm64OperandType, ArchOperand::Arm64Operand, BuildsCapstone},
    Capstone, Insn,
};
use color_backtrace::{default_output_stream, BacktracePrinter, Verbosity};
use dynasmrt::{dynasm, DynasmApi, DynasmLabelApi};
use frida_gum::{interceptor::Interceptor, Gum, ModuleMap};
#[cfg(unix)]
use libc::{c_char, getrlimit64, rlimit64, sysconf, wchar_t, _SC_PAGESIZE};
use rangemap::RangeMap;
use rangemap::RangeSet;
use serde::{Deserialize, Serialize};
use std::{
    cell::{RefCell, RefMut},
    ffi::c_void,
    io::{self, Write},
    path::PathBuf,
    rc::Rc,
};
use termcolor::{Color, ColorSpec, WriteColor};

use crate::FridaOptions;

extern "C" {
    fn __register_frame(begin: *mut c_void);
}

static mut ALLOCATOR_SINGLETON: Option<RefCell<Allocator>> = None;

struct Allocator {
    runtime: Rc<RefCell<AsanRuntime>>,
    page_size: usize,
    shadow_offset: usize,
    shadow_bit: usize,
    pre_allocated_shadow: bool,
    allocations: HashMap<usize, AllocationMetadata>,
    shadow_pages: RangeSet<usize>,
    allocation_queue: HashMap<usize, Vec<AllocationMetadata>>,
    largest_allocation: usize,
}

macro_rules! map_to_shadow {
    ($self:expr, $address:expr) => {
        (($address >> 3) + $self.shadow_offset) & ((1 << ($self.shadow_bit + 1)) - 1)
    };
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct AllocationMetadata {
    address: usize,
    size: usize,
    actual_size: usize,
    allocation_site_backtrace: Option<Backtrace>,
    release_site_backtrace: Option<Backtrace>,
    freed: bool,
    is_malloc_zero: bool,
}

impl Allocator {
    fn setup(runtime: Rc<RefCell<AsanRuntime>>) {
        let ret = unsafe { sysconf(_SC_PAGESIZE) };
        if ret < 0 {
            panic!("Failed to read pagesize {:?}", io::Error::last_os_error());
        }
        #[allow(clippy::cast_sign_loss)]
        let page_size = ret as usize;
        // probe to find a usable shadow bit:
        let mut shadow_bit: usize = 0;
        for try_shadow_bit in &[46usize, 36usize] {
            let addr: usize = 1 << try_shadow_bit;
            if unsafe {
                mmap(
                    addr as *mut c_void,
                    page_size,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_PRIVATE
                        | MapFlags::MAP_ANONYMOUS
                        | MapFlags::MAP_FIXED
                        | MapFlags::MAP_NORESERVE,
                    -1,
                    0,
                )
            }
            .is_ok()
            {
                shadow_bit = *try_shadow_bit;
                break;
            }
        }
        assert!(shadow_bit != 0);

        // attempt to pre-map the entire shadow-memory space
        let addr: usize = 1 << shadow_bit;
        let pre_allocated_shadow = unsafe {
            mmap(
                addr as *mut c_void,
                addr + addr,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANONYMOUS
                    | MapFlags::MAP_FIXED
                    | MapFlags::MAP_PRIVATE
                    | MapFlags::MAP_NORESERVE,
                -1,
                0,
            )
        }
        .is_ok();

        let allocator = Self {
            runtime,
            page_size,
            pre_allocated_shadow,
            shadow_offset: 1 << shadow_bit,
            shadow_bit,
            allocations: HashMap::new(),
            shadow_pages: RangeSet::new(),
            allocation_queue: HashMap::new(),
            largest_allocation: 0,
        };
        unsafe {
            ALLOCATOR_SINGLETON = Some(RefCell::new(allocator));
        }
    }

    pub fn get() -> RefMut<'static, Allocator> {
        unsafe {
            ALLOCATOR_SINGLETON
                .as_mut()
                .unwrap()
                .try_borrow_mut()
                .unwrap()
        }
    }

    pub fn init(runtime: Rc<RefCell<AsanRuntime>>) {
        Self::setup(runtime);
    }

    #[inline]
    fn round_up_to_page(&self, size: usize) -> usize {
        ((size + self.page_size) / self.page_size) * self.page_size
    }

    #[inline]
    fn round_down_to_page(&self, value: usize) -> usize {
        (value / self.page_size) * self.page_size
    }

    fn find_smallest_fit(&mut self, size: usize) -> Option<AllocationMetadata> {
        let mut current_size = size;
        while current_size <= self.largest_allocation {
            if self.allocation_queue.contains_key(&current_size) {
                if let Some(metadata) = self.allocation_queue.entry(current_size).or_default().pop()
                {
                    return Some(metadata);
                }
            }
            current_size *= 2;
        }
        None
    }

    pub unsafe fn alloc(&mut self, size: usize, _alignment: usize) -> *mut c_void {
        let mut is_malloc_zero = false;
        let size = if size == 0 {
            println!("zero-sized allocation!");
            is_malloc_zero = true;
            16
        } else {
            size
        };
        if size > (1 << 30) {
            panic!("Allocation is too large: 0x{:x}", size);
        }
        let rounded_up_size = self.round_up_to_page(size) + 2 * self.page_size;

        let metadata = if let Some(mut metadata) = self.find_smallest_fit(rounded_up_size) {
            //println!("reusing allocation at {:x}, (actual mapping starts at {:x}) size {:x}", metadata.address, metadata.address - self.page_size, size);
            metadata.is_malloc_zero = is_malloc_zero;
            metadata.size = size;
            if self
                .runtime
                .borrow()
                .options
                .enable_asan_allocation_backtraces
            {
                metadata.allocation_site_backtrace = Some(Backtrace::new_unresolved());
            }
            metadata
        } else {
            let mapping = match mmap(
                std::ptr::null_mut(),
                rounded_up_size,
                ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                MapFlags::MAP_ANONYMOUS | MapFlags::MAP_PRIVATE,
                -1,
                0,
            ) {
                Ok(mapping) => mapping as usize,
                Err(err) => {
                    println!("An error occurred while mapping memory: {:?}", err);
                    return std::ptr::null_mut();
                }
            };

            self.map_shadow_for_region(mapping, mapping + rounded_up_size, false);

            let mut metadata = AllocationMetadata {
                address: mapping,
                size,
                actual_size: rounded_up_size,
                ..AllocationMetadata::default()
            };

            if self
                .runtime
                .borrow()
                .options
                .enable_asan_allocation_backtraces
            {
                metadata.allocation_site_backtrace = Some(Backtrace::new_unresolved());
            }

            metadata
        };

        self.largest_allocation = std::cmp::max(self.largest_allocation, metadata.actual_size);
        // unpoison the shadow memory for the allocation itself
        Self::unpoison(map_to_shadow!(self, metadata.address + self.page_size), size);
        let address = (metadata.address + self.page_size) as *mut c_void;

        self.allocations.insert(metadata.address + self.page_size, metadata);
        //println!("serving address: {:?}, size: {:x}", address, size);
        address
    }

    pub unsafe fn release(&mut self, ptr: *mut c_void) {
        let mut metadata = if let Some(metadata) = self.allocations.get_mut(&(ptr as usize)) {
            metadata
        } else {
            if !ptr.is_null() {
                // TODO: report this as an observer
                self.runtime
                    .borrow_mut()
                    .report_error(AsanError::UnallocatedFree((ptr as usize, Backtrace::new())));
            }
            return;
        };

        if metadata.freed {
            self.runtime
                .borrow_mut()
                .report_error(AsanError::DoubleFree((
                    ptr as usize,
                    metadata.clone(),
                    Backtrace::new(),
                )));
        }
        let shadow_mapping_start = map_to_shadow!(self, ptr as usize);

        metadata.freed = true;
        if self
            .runtime
            .borrow()
            .options
            .enable_asan_allocation_backtraces
        {
            metadata.release_site_backtrace = Some(Backtrace::new_unresolved());
        }

        // poison the shadow memory for the allocation
        Self::poison(shadow_mapping_start, metadata.size);
    }

    pub fn find_metadata(
        &mut self,
        ptr: usize,
        hint_base: usize,
    ) -> Option<&mut AllocationMetadata> {
        let mut metadatas: Vec<&mut AllocationMetadata> = self.allocations.values_mut().collect();
        metadatas.sort_by(|a, b| a.address.cmp(&b.address));
        let mut offset_to_closest = i64::max_value();
        let mut closest = None;
        for metadata in metadatas {
            let new_offset = if hint_base == metadata.address {
                (ptr as i64 - metadata.address as i64).abs()
            } else {
                std::cmp::min(
                    offset_to_closest,
                    (ptr as i64 - metadata.address as i64).abs(),
                )
            };
            if new_offset < offset_to_closest {
                offset_to_closest = new_offset;
                closest = Some(metadata);
            }
        }
        closest
    }

    pub fn reset(&mut self) {
        for (address, mut allocation) in self.allocations.drain() {
            // First poison the memory.
            Self::poison(map_to_shadow!(self, address), allocation.size);

            // Reset the allocaiton metadata object
            allocation.size = 0;
            allocation.freed = false;
            allocation.allocation_site_backtrace = None;
            allocation.release_site_backtrace = None;

            // Move the allocation from the allocations to the to-be-allocated queues
            self.allocation_queue
                .entry(allocation.actual_size)
                .or_default()
                .push(allocation);
        }
    }

    pub fn get_usable_size(&self, ptr: *mut c_void) -> usize {
        match self.allocations.get(&(ptr as usize)) {
            Some(metadata) => metadata.size,
            None => {
                panic!(
                    "Attempted to get_usable_size on a pointer ({:?}) which was not allocated!",
                    ptr
                );
            }
        }
    }

    fn unpoison(start: usize, size: usize) {
        //println!("unpoisoning {:x} for {:x}", start, size / 8 + 1);
        unsafe {
            //println!("memset: {:?}", start as *mut c_void);
            memset(start as *mut c_void, 0xff, size / 8);

            let remainder = size % 8;
            if remainder > 0 {
                //println!("remainder: {:x}, offset: {:x}", remainder, start + size / 8);
                memset(
                    (start + size / 8) as *mut c_void,
                    (0xff << (8 - remainder)) & 0xff,
                    1,
                );
            }
        }
    }

    pub fn poison(start: usize, size: usize) {
        //println!("poisoning {:x} for {:x}", start, size / 8 + 1);
        unsafe {
            //println!("memset: {:?}", start as *mut c_void);
            memset(start as *mut c_void, 0x00, size / 8);

            let remainder = size % 8;
            if remainder > 0 {
                //println!("remainder: {:x}, offset: {:x}", remainder, start + size / 8);
                memset((start + size / 8) as *mut c_void, 0x00, 1);
            }
        }
    }

    /// Map shadow memory for a region, and optionally unpoison it
    pub fn map_shadow_for_region(
        &mut self,
        start: usize,
        end: usize,
        unpoison: bool,
    ) -> (usize, usize) {
        //println!("start: {:x}, end {:x}, size {:x}", start, end, end - start);

        let shadow_mapping_start = map_to_shadow!(self, start);

        if !self.pre_allocated_shadow {
            let shadow_start = self.round_down_to_page(shadow_mapping_start);
            let shadow_end =
                self.round_up_to_page((end - start) / 8) + self.page_size + shadow_start;
            for range in self.shadow_pages.gaps(&(shadow_start..shadow_end)) {
                //println!("range: {:x}-{:x}, pagesize: {}", range.start, range.end, self.page_size);
                unsafe {
                    mmap(
                        range.start as *mut c_void,
                        range.end - range.start,
                        ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                        MapFlags::MAP_ANONYMOUS | MapFlags::MAP_FIXED | MapFlags::MAP_PRIVATE,
                        -1,
                        0,
                    )
                    .expect("An error occurred while mapping shadow memory");
                }
            }

            self.shadow_pages.insert(shadow_start..shadow_end);
        }

        //println!("shadow_mapping_start: {:x}, shadow_size: {:x}", shadow_mapping_start, (end - start) / 8);
        if unpoison {
            Self::unpoison(shadow_mapping_start, end - start);
        }

        (shadow_mapping_start, (end - start) / 8)
    }
}

/// Get the current thread's TLS address
extern "C" {
    fn tls_ptr() -> *const c_void;
}

/// The frida address sanitizer runtime, providing address sanitization.
/// When executing in `ASAN`, each memory access will get checked, using frida stalker under the hood.
/// The runtime can report memory errors that occurred during execution,
/// even if the target would not have crashed under normal conditions.
/// this helps finding mem errors early.
pub struct AsanRuntime {
    regs: [usize; 32],
    blob_report: Option<Box<[u8]>>,
    blob_check_mem_byte: Option<Box<[u8]>>,
    blob_check_mem_halfword: Option<Box<[u8]>>,
    blob_check_mem_dword: Option<Box<[u8]>>,
    blob_check_mem_qword: Option<Box<[u8]>>,
    blob_check_mem_16bytes: Option<Box<[u8]>>,
    blob_check_mem_3bytes: Option<Box<[u8]>>,
    blob_check_mem_6bytes: Option<Box<[u8]>>,
    blob_check_mem_12bytes: Option<Box<[u8]>>,
    blob_check_mem_24bytes: Option<Box<[u8]>>,
    blob_check_mem_32bytes: Option<Box<[u8]>>,
    blob_check_mem_48bytes: Option<Box<[u8]>>,
    blob_check_mem_64bytes: Option<Box<[u8]>>,
    stalked_addresses: HashMap<usize, usize>,
    options: FridaOptions,
    instrumented_ranges: RangeMap<usize, String>,
    module_map: Option<ModuleMap>,
    shadow_check_func: Option<extern "C" fn(*const c_void, usize) -> bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct AsanReadWriteError {
    registers: [usize; 32],
    pc: usize,
    fault: (u16, u16, usize, usize),
    metadata: AllocationMetadata,
    backtrace: Backtrace,
}

#[derive(Debug, Clone, Serialize, Deserialize, SerdeAny)]
enum AsanError {
    OobRead(AsanReadWriteError),
    OobWrite(AsanReadWriteError),
    ReadAfterFree(AsanReadWriteError),
    WriteAfterFree(AsanReadWriteError),
    DoubleFree((usize, AllocationMetadata, Backtrace)),
    UnallocatedFree((usize, Backtrace)),
    Unknown(([usize; 32], usize, (u16, u16, usize, usize), Backtrace)),
    Leak((usize, AllocationMetadata)),
    StackOobRead(([usize; 32], usize, (u16, u16, usize, usize), Backtrace)),
    StackOobWrite(([usize; 32], usize, (u16, u16, usize, usize), Backtrace)),
    BadFuncArgRead((String, usize, usize, Backtrace)),
    BadFuncArgWrite((String, usize, usize, Backtrace)),
}

impl AsanError {
    fn description(&self) -> &str {
        match self {
            AsanError::OobRead(_) => "heap out-of-bounds read",
            AsanError::OobWrite(_) => "heap out-of-bounds write",
            AsanError::DoubleFree(_) => "double-free",
            AsanError::UnallocatedFree(_) => "unallocated-free",
            AsanError::WriteAfterFree(_) => "heap use-after-free write",
            AsanError::ReadAfterFree(_) => "heap use-after-free read",
            AsanError::Unknown(_) => "heap unknown",
            AsanError::Leak(_) => "memory-leak",
            AsanError::StackOobRead(_) => "stack out-of-bounds read",
            AsanError::StackOobWrite(_) => "stack out-of-bounds write",
            AsanError::BadFuncArgRead(_) => "function arg resulting in bad read",
            AsanError::BadFuncArgWrite(_) => "function arg resulting in bad write",
        }
    }
}

/// A struct holding errors that occurred during frida address sanitizer runs
#[derive(Debug, Clone, Serialize, Deserialize, SerdeAny)]
pub struct AsanErrors {
    errors: Vec<AsanError>,
}

impl AsanErrors {
    /// Creates a new `AsanErrors` struct
    #[must_use]
    fn new() -> Self {
        Self { errors: Vec::new() }
    }

    /// Clears this `AsanErrors` struct
    pub fn clear(&mut self) {
        self.errors.clear()
    }

    /// Gets the amount of `AsanErrors` in this struct
    #[must_use]
    pub fn len(&self) -> usize {
        self.errors.len()
    }

    /// Returns `true` if no errors occurred
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.errors.is_empty()
    }
}
impl CustomExitKind for AsanErrors {}

impl AsanRuntime {
    /// Create a new `AsanRuntime`
    #[must_use]
    pub fn new(options: FridaOptions) -> Rc<RefCell<AsanRuntime>> {
        let res = Rc::new(RefCell::new(Self {
            regs: [0; 32],
            blob_report: None,
            blob_check_mem_byte: None,
            blob_check_mem_halfword: None,
            blob_check_mem_dword: None,
            blob_check_mem_qword: None,
            blob_check_mem_16bytes: None,
            blob_check_mem_3bytes: None,
            blob_check_mem_6bytes: None,
            blob_check_mem_12bytes: None,
            blob_check_mem_24bytes: None,
            blob_check_mem_32bytes: None,
            blob_check_mem_48bytes: None,
            blob_check_mem_64bytes: None,
            stalked_addresses: HashMap::new(),
            options,
            instrumented_ranges: RangeMap::new(),
            module_map: None,
            shadow_check_func: None,
        }));
        Allocator::init(res.clone());
        res
    }
    /// Initialize the runtime so that it is read for action. Take care not to move the runtime
    /// instance after this function has been called, as the generated blobs would become
    /// invalid!
    pub fn init(&mut self, gum: &Gum, modules_to_instrument: &[PathBuf]) {
        unsafe {
            ASAN_ERRORS = Some(AsanErrors::new());
        }

        self.generate_instrumentation_blobs();
        self.generate_shadow_check_function();
        self.unpoison_all_existing_memory();

        for module_name in modules_to_instrument {
            let (start, end) = find_mapping_for_path(module_name.to_str().unwrap());
            self.instrumented_ranges
                .insert(start..end, module_name.to_str().unwrap().to_string());
        }
        let module_names: Vec<&str> = modules_to_instrument
            .iter()
            .map(|modname| modname.to_str().unwrap())
            .collect();
        self.module_map = Some(ModuleMap::new_from_names(&module_names));
        self.hook_functions(gum);
        //unsafe {
            //let mem = Allocator::get().alloc(0xac + 2, 8);

            //unsafe {mprotect((self.shadow_check_func.unwrap() as usize & 0xffffffffffff000) as *mut c_void, 0x1000, ProtFlags::PROT_READ | ProtFlags::PROT_WRITE | ProtFlags::PROT_EXEC)};
            //assert!((self.shadow_check_func.unwrap())(((mem as usize) + 0) as *const c_void, 0xac));
            //assert!((self.shadow_check_func.unwrap())(((mem as usize) + 2) as *const c_void, 0xac));
            //assert!(!(self.shadow_check_func.unwrap())(((mem as usize) + 3) as *const c_void, 0xac));
            //assert!(!(self.shadow_check_func.unwrap())(((mem as isize) + -1) as *const c_void, 0xac));
            //assert!((self.shadow_check_func.unwrap())(((mem as usize) + 2 + 0xa4) as *const c_void, 8));
            //assert!((self.shadow_check_func.unwrap())(((mem as usize) + 2 + 0xa6) as *const c_void, 6));
            //assert!(!(self.shadow_check_func.unwrap())(((mem as usize) + 2 + 0xa8) as *const c_void, 6));
            //assert!(!(self.shadow_check_func.unwrap())(((mem as usize) + 2 + 0xa8) as *const c_void, 0xac));
            //assert!((self.shadow_check_func.unwrap())(((mem as usize) + 4 + 0xa8) as *const c_void, 0x1));
        //}
    }

    /// Reset all allocations so that they can be reused for new allocation requests.
    #[allow(clippy::unused_self)]
    pub fn reset_allocations(&self) {
        Allocator::get().reset();
    }

    /// Check if the test leaked any memory and report it if so.
    pub fn check_for_leaks(&mut self) {
        for metadata in Allocator::get().allocations.values_mut() {
            if !metadata.freed {
                self.report_error(AsanError::Leak((metadata.address, metadata.clone())));
            }
        }
    }

    /// Returns the `AsanErrors` from the recent run
    #[allow(clippy::unused_self)]
    pub fn errors(&mut self) -> &Option<AsanErrors> {
        unsafe { &ASAN_ERRORS }
    }

    /// Make sure the specified memory is unpoisoned
    #[allow(clippy::unused_self)]
    pub fn unpoison(&self, address: usize, size: usize) {
        Allocator::get().map_shadow_for_region(address, address + size, true);
    }

    /// Add a stalked address to real address mapping.
    #[inline]
    pub fn add_stalked_address(&mut self, stalked: usize, real: usize) {
        self.stalked_addresses.insert(stalked, real);
    }

    /// Resolves the real address from a stalker stalked address
    #[must_use]
    pub fn real_address_for_stalked(&self, stalked: usize) -> Option<&usize> {
        self.stalked_addresses.get(&stalked)
    }

    /// Unpoison all the memory that is currently mapped with read/write permissions.
    #[allow(clippy::unused_self)]
    fn unpoison_all_existing_memory(&self) {
        let mut allocator = Allocator::get();
        walk_self_maps(&mut |start, end, permissions, _path| {
            if permissions.as_bytes()[0] == b'r' || permissions.as_bytes()[1] == b'w' {
                if allocator.pre_allocated_shadow && start == 1 << allocator.shadow_bit {
                    return false;
                }
                allocator.map_shadow_for_region(start, end, true);
            }
            false
        });
    }

    /// Register the current thread with the runtime, implementing shadow memory for its stack and
    /// tls mappings.
    #[allow(clippy::unused_self)]
    pub fn register_thread(&self) {
        let mut allocator = Allocator::get();
        let (stack_start, stack_end) = Self::current_stack();
        allocator.map_shadow_for_region(stack_start, stack_end, true);

        let (tls_start, tls_end) = Self::current_tls();
        allocator.map_shadow_for_region(tls_start, tls_end, true);
        println!(
            "registering thread with stack {:x}:{:x} and tls {:x}:{:x}",
            stack_start as usize, stack_end as usize, tls_start as usize, tls_end as usize
        );
    }

    /// Determine the stack start, end for the currently running thread
    ///
    /// # Panics
    /// Panics, if no mapping for the `stack_address` at `0xeadbeef` could be found.
    #[must_use]
    pub fn current_stack() -> (usize, usize) {
        let stack_var = 0xeadbeef;
        let stack_address = &stack_var as *const _ as *const c_void as usize;
        let (start, end, _, _) = find_mapping_for_address(stack_address).unwrap();

        let mut stack_rlimit = rlimit64 {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert!(unsafe { getrlimit64(3, &mut stack_rlimit as *mut rlimit64) } == 0);

        let max_start = end - stack_rlimit.rlim_cur as usize;

        if start != max_start {
            let mapping = unsafe {
                mmap(
                    max_start as *mut c_void,
                    start - max_start,
                    ProtFlags::PROT_READ | ProtFlags::PROT_WRITE,
                    MapFlags::MAP_ANONYMOUS
                        | MapFlags::MAP_FIXED
                        | MapFlags::MAP_PRIVATE
                        | MapFlags::MAP_STACK,
                    -1,
                    0,
                )
            };
            assert!(mapping.unwrap() as usize == max_start);
        }
        (max_start, end)
    }

    /// Determine the tls start, end for the currently running thread
    fn current_tls() -> (usize, usize) {
        let tls_address = unsafe { tls_ptr() } as usize;

        #[cfg(target_os = "android")]
        let tls_address = tls_address & 0xffffffffffffff;

        let (start, end, _, _) = find_mapping_for_address(tls_address).unwrap();
        (start, end)
    }

    #[inline]
    fn hook_malloc(&mut self, size: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, 8) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__Znam(&mut self, size: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, 8) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnamRKSt9nothrow_t(&mut self, size: usize, _nothrow: *const c_void) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, 8) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnamSt11align_val_t(&mut self, size: usize, alignment: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, alignment) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnamSt11align_val_tRKSt9nothrow_t(
        &mut self,
        size: usize,
        alignment: usize,
        _nothrow: *const c_void,
    ) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, alignment) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__Znwm(&mut self, size: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, 8) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnwmRKSt9nothrow_t(&mut self, size: usize, _nothrow: *const c_void) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, 8) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnwmSt11align_val_t(&mut self, size: usize, alignment: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, alignment) }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZnwmSt11align_val_tRKSt9nothrow_t(
        &mut self,
        size: usize,
        alignment: usize,
        _nothrow: *const c_void,
    ) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, alignment) }
    }

    #[inline]
    fn hook_calloc(&mut self, nmemb: usize, size: usize) -> *mut c_void {
        let ret = unsafe { Allocator::get().alloc(size * nmemb, 8) };
        unsafe {
            memset(ret, 0, size * nmemb);
        }
        ret
    }

    #[inline]
    fn hook_realloc(&mut self, ptr: *mut c_void, size: usize) -> *mut c_void {
        unsafe {
            let mut allocator = Allocator::get();
            let ret = allocator.alloc(size, 0x8);
            if ptr != std::ptr::null_mut() {
                memmove(ret, ptr, allocator.get_usable_size(ptr));
            }
            allocator.release(ptr);
            ret
        }
    }

    #[inline]
    fn hook_free(&mut self, ptr: *mut c_void) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[inline]
    fn hook_memalign(&mut self, size: usize, alignment: usize) -> *mut c_void {
        unsafe { Allocator::get().alloc(size, alignment) }
    }

    #[inline]
    fn hook_posix_memalign(
        &mut self,
        pptr: *mut *mut c_void,
        size: usize,
        alignment: usize,
    ) -> i32 {
        unsafe {
            *pptr = Allocator::get().alloc(size, alignment);
        }
        0
    }

    #[inline]
    fn hook_malloc_usable_size(&mut self, ptr: *mut c_void) -> usize {
        Allocator::get().get_usable_size(ptr)
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPv(&mut self, ptr: *mut c_void) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPvm(&mut self, ptr: *mut c_void, _ulong: u64) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPvmSt11align_val_t(&mut self, ptr: *mut c_void, _ulong: u64, _alignment: usize) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPvRKSt9nothrow_t(&mut self, ptr: *mut c_void, _nothrow: *const c_void) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPvSt11align_val_tRKSt9nothrow_t(
        &mut self,
        ptr: *mut c_void,
        _alignment: usize,
        _nothrow: *const c_void,
    ) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdaPvSt11align_val_t(&mut self, ptr: *mut c_void, _alignment: usize) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPv(&mut self, ptr: *mut c_void) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPvm(&mut self, ptr: *mut c_void, _ulong: u64) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPvmSt11align_val_t(&mut self, ptr: *mut c_void, _ulong: u64, _alignment: usize) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPvRKSt9nothrow_t(&mut self, ptr: *mut c_void, _nothrow: *const c_void) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPvSt11align_val_tRKSt9nothrow_t(
        &mut self,
        ptr: *mut c_void,
        _alignment: usize,
        _nothrow: *const c_void,
    ) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    #[allow(non_snake_case)]
    #[inline]
    fn hook__ZdlPvSt11align_val_t(&mut self, ptr: *mut c_void, _alignment: usize) {
        if ptr != std::ptr::null_mut() {
            unsafe { Allocator::get().release(ptr) }
        }
    }

    fn hook_mmap(&mut self, addr: *const c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: usize) -> *mut c_void {
        extern "C" {
            fn mmap(addr: *const c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: usize) -> *mut c_void;
        }
        let res = unsafe { mmap(addr, length, prot, flags, fd, offset) };
        if res != (-1isize as *mut c_void) {
            Allocator::get().map_shadow_for_region(res as usize, res as usize + length, true);
        }
        res
    }

    fn hook_munmap(&mut self, addr: *const c_void, length: usize) -> i32 {
        extern "C" {
            fn munmap(addr: *const c_void, length: usize) -> i32;
        }
        let res = unsafe { munmap(addr, length) };
        if res != -1 {
            let allocator = Allocator::get();
            Allocator::poison(map_to_shadow!(allocator, addr as usize), length);
        }
        res
    }

    #[inline]
    fn hook_write(&mut self, fd: i32, buf: *const c_void, count: usize) -> usize {
        extern "C" {
            fn write(fd: i32, buf: *const c_void, count: usize) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(buf, count) {
            self.report_error(AsanError::BadFuncArgWrite(("write".to_string(), buf as usize, count, Backtrace::new())));
        }
        unsafe {
            write(fd, buf, count)
        }
    }

    #[inline]
    fn hook_read(&mut self, fd: i32, buf: *mut c_void, count: usize) -> usize {
        extern "C" {
            fn read(fd: i32, buf: *mut c_void, count: usize) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(buf, count) {
            self.report_error(AsanError::BadFuncArgRead(("read".to_string(), buf as usize, count, Backtrace::new())));
        }
        unsafe {
            read(fd, buf, count)
        }
    }

    #[inline]
    fn hook_fgets(&mut self, s: *mut c_void, size: u32, stream: *mut c_void) -> *mut c_void {
        extern "C" {
            fn fgets(s: *mut c_void, size: u32, stream: *mut c_void) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(s, size as usize) {
            self.report_error(AsanError::BadFuncArgRead(("fgets".to_string(), s as usize, size as usize, Backtrace::new())));
        }
        unsafe {
            fgets(s, size, stream)
        }
    }

    #[inline]
    fn hook_memcmp(&mut self, s1: *const c_void, s2: *const c_void, n: usize) -> i32 {
        extern "C" {
            fn memcmp(s1: *const c_void, s2: *const c_void, n: usize) -> i32;
        }
        if !(self.shadow_check_func.unwrap())(s1, n) {
            self.report_error(AsanError::BadFuncArgRead(("memcmp".to_string(), s1 as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2, n) {
            self.report_error(AsanError::BadFuncArgRead(("memcmp".to_string(), s2 as usize, n, Backtrace::new())));
        }
        unsafe {
            memcmp(s1, s2, n)
        }
    }

    #[inline]
    fn hook_memcpy(&mut self, dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        extern "C" {
            fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(dest, n) {
            self.report_error(AsanError::BadFuncArgWrite(("memcpy".to_string(), dest as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src, n) {
            self.report_error(AsanError::BadFuncArgRead(("memcpy".to_string(), src as usize, n, Backtrace::new())));
        }
        unsafe {
            memcpy(dest, src, n)
        }
    }

    #[inline]
    fn hook_mempcpy(&mut self, dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        extern "C" {
            fn mempcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(dest, n) {
            self.report_error(AsanError::BadFuncArgWrite(("mempcpy".to_string(), dest as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src, n) {
            self.report_error(AsanError::BadFuncArgRead(("mempcpy".to_string(), src as usize, n, Backtrace::new())));
        }
        unsafe {
            mempcpy(dest, src, n)
        }
    }

    #[inline]
    fn hook_memmove(&mut self, dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void {
        extern "C" {
            fn memmove(dest: *mut c_void, src: *const c_void, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(dest, n) {
            self.report_error(AsanError::BadFuncArgWrite(("memmove".to_string(), dest as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src, n) {
            self.report_error(AsanError::BadFuncArgRead(("memmove".to_string(), src as usize, n, Backtrace::new())));
        }
        unsafe {
            memmove(dest, src, n)
        }
    }

    #[inline]
    fn hook_memset(&mut self, dest: *mut c_void, c: i32, n: usize) -> *mut c_void {
        extern "C" {
            fn memset(dest: *mut c_void, c: i32, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(dest, n) {
            self.report_error(AsanError::BadFuncArgWrite(("memset".to_string(), dest as usize, n, Backtrace::new())));
        }
        unsafe {
            memset(dest, c, n)
        }
    }

    #[inline]
    fn hook_memchr(&mut self, s: *mut c_void, c: i32, n: usize) -> *mut c_void {
        extern "C" {
            fn memchr(s: *mut c_void, c: i32, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(s, n) {
            self.report_error(AsanError::BadFuncArgRead(("memchr".to_string(), s as usize, n, Backtrace::new())));
        }
        unsafe {
            memchr(s, c, n)
        }
    }

    #[inline]
    fn hook_memrchr(&mut self, s: *mut c_void, c: i32, n: usize) -> *mut c_void {
        extern "C" {
            fn memrchr(s: *mut c_void, c: i32, n: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(s, n) {
            self.report_error(AsanError::BadFuncArgRead(("memrchr".to_string(), s as usize, n, Backtrace::new())));
        }
        unsafe {
            memrchr(s, c, n)
        }
    }

    #[inline]
    fn hook_memmem(&mut self, haystack: *const c_void, haystacklen: usize, needle: *const c_void, needlelen: usize) -> *mut c_void {
        extern "C" {
            fn memmem(haystack: *const c_void, haystacklen: usize, needle: *const c_void, needlelen: usize) -> *mut c_void;
        }
        if !(self.shadow_check_func.unwrap())(haystack, haystacklen) {
            self.report_error(AsanError::BadFuncArgRead(("memmem".to_string(), haystack as usize, haystacklen, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(needle, needlelen) {
            self.report_error(AsanError::BadFuncArgRead(("memmem".to_string(), needle as usize, needlelen, Backtrace::new())));
        }
        unsafe {
            memmem(haystack, haystacklen, needle, needlelen)
        }
    }

    #[cfg(not(target_os = "android"))]
    #[inline]
    fn hook_bzero(&mut self, s: *mut c_void, n: usize) {
        extern "C" {
            fn bzero(s: *mut c_void, n: usize);
        }
        if !(self.shadow_check_func.unwrap())(s, n) {
            self.report_error(AsanError::BadFuncArgWrite(("bzero".to_string(), s as usize, n, Backtrace::new())));
        }
        unsafe {
            bzero(s, n)
        }
    }

    #[cfg(not(target_os = "android"))]
    #[inline]
    fn hook_explicit_bzero(&mut self, s: *mut c_void, n: usize) {
        extern "C" {
            fn explicit_bzero(s: *mut c_void, n: usize);
        }
        if !(self.shadow_check_func.unwrap())(s, n) {
            self.report_error(AsanError::BadFuncArgWrite(("explicit_bzero".to_string(), s as usize, n, Backtrace::new())));
        }
        unsafe {
            explicit_bzero(s, n)
        }
    }

    #[cfg(not(target_os = "android"))]
    #[inline]
    fn hook_bcmp(&mut self, s1: *const c_void, s2: *const c_void, n: usize) -> i32 {
        extern "C" {
            fn bcmp(s1: *const c_void, s2: *const c_void, n: usize) -> i32;
        }
        if !(self.shadow_check_func.unwrap())(s1, n) {
            self.report_error(AsanError::BadFuncArgRead(("bcmp".to_string(), s1 as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2, n) {
            self.report_error(AsanError::BadFuncArgRead(("bcmp".to_string(), s2 as usize, n, Backtrace::new())));
        }
        unsafe {
            bcmp(s1, s2, n)
        }
    }

    #[inline]
    fn hook_strchr(&mut self, s: *mut c_char, c: i32) -> *mut c_char {
        extern "C" {
            fn strchr(s: *mut c_char, c: i32) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("strchr".to_string(), s as usize, unsafe { strlen(s) }, Backtrace::new())));
        }
        unsafe {
            strchr(s, c)
        }
    }

    #[inline]
    fn hook_strrchr(&mut self, s: *mut c_char, c: i32) -> *mut c_char {
        extern "C" {
            fn strrchr(s: *mut c_char, c: i32) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("strrchr".to_string(), s as usize, unsafe { strlen(s) }, Backtrace::new())));
        }
        unsafe {
            strrchr(s, c)
        }
    }

    #[inline]
    fn hook_strcasecmp(&mut self, s1: *const c_char, s2: *const c_char) -> i32 {
        extern "C" {
            fn strcasecmp(s1: *const c_char, s2: *const c_char) -> i32;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, unsafe { strlen(s1) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcasecmp".to_string(), s1 as usize, unsafe { strlen(s1) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, unsafe { strlen(s2) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcasecmp".to_string(), s2 as usize, unsafe { strlen(s2) }, Backtrace::new())));
        }
        unsafe {
            strcasecmp(s1, s2)
        }
    }

    #[inline]
    fn hook_strncasecmp(&mut self, s1: *const c_char, s2: *const c_char, n: usize) -> i32 {
        extern "C" {
            fn strncasecmp(s1: *const c_char, s2: *const c_char, n: usize) -> i32;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgRead(("strncasecmp".to_string(), s1 as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgRead(("strncasecmp".to_string(), s2 as usize, n, Backtrace::new())));
        }
        unsafe {
            strncasecmp(s1, s2, n)
        }
    }

    #[inline]
    fn hook_strcat(&mut self, s1: *mut c_char, s2: *const c_char) -> *mut c_char {
        extern "C" {
            fn strcat(s1: *mut c_char, s2: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, unsafe { strlen(s1) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcat".to_string(), s1 as usize, unsafe { strlen(s1) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, unsafe { strlen(s2) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcat".to_string(), s2 as usize, unsafe { strlen(s2) }, Backtrace::new())));
        }
        unsafe {
            strcat(s1, s2)
        }
    }

    #[inline]
    fn hook_strcmp(&mut self, s1: *const c_char, s2: *const c_char) -> i32 {
        extern "C" {
            fn strcmp(s1: *const c_char, s2: *const c_char) -> i32;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, unsafe { strlen(s1) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcmp".to_string(), s1 as usize, unsafe { strlen(s1) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, unsafe { strlen(s2) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcmp".to_string(), s2 as usize, unsafe { strlen(s2) }, Backtrace::new())));
        }
        unsafe {
            strcmp(s1, s2)
        }
    }

    #[inline]
    fn hook_strncmp(&mut self, s1: *const c_char, s2: *const c_char, n: usize) -> i32 {
        extern "C" {
            fn strncmp(s1: *const c_char, s2: *const c_char, n: usize) -> i32;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgRead(("strncmp".to_string(), s1 as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgRead(("strncmp".to_string(), s2 as usize, n, Backtrace::new())));
        }
        unsafe {
            strncmp(s1, s2, n)
        }
    }

    #[inline]
    fn hook_strcpy(&mut self, dest: *mut c_char, src: *const c_char) -> *mut c_char {
        extern "C" {
            fn strcpy(dest: *mut c_char, src: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(dest as *const c_void, unsafe { strlen(src) }) {
            self.report_error(AsanError::BadFuncArgWrite(("strcpy".to_string(), dest as usize, unsafe { strlen(src) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src as *const c_void, unsafe { strlen(src) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcpy".to_string(), src as usize, unsafe { strlen(src) }, Backtrace::new())));
        }
        unsafe {
            strcpy(dest, src)
        }
    }

    #[inline]
    fn hook_strncpy(&mut self, dest: *mut c_char, src: *const c_char, n: usize) -> *mut c_char {
        extern "C" {
            fn strncpy(dest: *mut c_char, src: *const c_char, n: usize) -> *mut c_char;
        }
        if !(self.shadow_check_func.unwrap())(dest as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgWrite(("strncpy".to_string(), dest as usize, n, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src as *const c_void, n) {
            self.report_error(AsanError::BadFuncArgRead(("strncpy".to_string(), src as usize, n, Backtrace::new())));
        }
        unsafe {
            strncpy(dest, src, n)
        }
    }

    #[inline]
    fn hook_stpcpy(&mut self, dest: *mut c_char, src: *const c_char) -> *mut c_char {
        extern "C" {
            fn stpcpy(dest: *mut c_char, src: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(dest as *const c_void, unsafe { strlen(src) }) {
            self.report_error(AsanError::BadFuncArgWrite(("stpcpy".to_string(), dest as usize, unsafe { strlen(src) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src as *const c_void, unsafe { strlen(src) }) {
            self.report_error(AsanError::BadFuncArgRead(("stpcpy".to_string(), src as usize, unsafe { strlen(src) }, Backtrace::new())));
        }
        unsafe {
            stpcpy(dest, src)
        }
    }

    #[inline]
    fn hook_strdup(&mut self, s: *const c_char) -> *mut c_char {
        extern "C" {
            fn strdup(s: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("strdup".to_string(), s as usize, unsafe { strlen(s) }, Backtrace::new())));
        }
        unsafe {
            strdup(s)
        }
    }

    #[inline]
    fn hook_strlen(&mut self, s: *const c_char) -> usize {
        extern "C" {
            fn strlen(s: *const c_char) -> usize;
        }
        let size = unsafe { strlen(s) };
        if !(self.shadow_check_func.unwrap())(s as *const c_void, size) {
            self.report_error(AsanError::BadFuncArgRead(("strlen".to_string(), s as usize, size, Backtrace::new())));
        }
        size
    }

    #[inline]
    fn hook_strnlen(&mut self, s: *const c_char, n: usize) -> usize {
        extern "C" {
            fn strnlen(s: *const c_char, n: usize) -> usize;
        }
        let size = unsafe { strnlen(s, n) };
        if !(self.shadow_check_func.unwrap())(s as *const c_void, size) {
            self.report_error(AsanError::BadFuncArgRead(("strnlen".to_string(), s as usize, size, Backtrace::new())));
        }
        size
    }

    #[inline]
    fn hook_strstr(&mut self, haystack: *const c_char, needle: *const c_char) -> *mut c_char {
        extern "C" {
            fn strstr(haystack: *const c_char, needle: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(haystack as *const c_void, unsafe { strlen(haystack) }) {
            self.report_error(AsanError::BadFuncArgRead(("strstr".to_string(), haystack as usize, unsafe { strlen(haystack) }, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(needle as *const c_void, unsafe { strlen(needle) }) {
            self.report_error(AsanError::BadFuncArgRead(("strstr".to_string(), needle as usize, unsafe { strlen(needle) }, Backtrace::new())));
        }
        unsafe {
            strstr(haystack, needle)
        }
    }

    #[inline]
    fn hook_strcasestr(&mut self, haystack: *const c_char, needle: *const c_char) -> *mut c_char {
        extern "C" {
            fn strcasestr(haystack: *const c_char, needle: *const c_char) -> *mut c_char;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(haystack as *const c_void, unsafe { strlen(haystack) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcasestr".to_string(), haystack as usize, unsafe {strlen(haystack)}, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(needle as *const c_void, unsafe { strlen(needle) }) {
            self.report_error(AsanError::BadFuncArgRead(("strcasestr".to_string(), needle as usize, unsafe {strlen(needle)}, Backtrace::new())));
        }
        unsafe {
            strcasestr(haystack, needle)
        }
    }

    #[inline]
    fn hook_atoi(&mut self, s: *const c_char) -> i32 {
        extern "C" {
            fn atoi(s: *const c_char) -> i32;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("atoi".to_string(), s as usize, unsafe {strlen(s)}, Backtrace::new())));
        }
        unsafe {
            atoi(s)
        }
    }

    #[inline]
    fn hook_atol(&mut self, s: *const c_char) -> i32 {
        extern "C" {
            fn atol(s: *const c_char) -> i32;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("atol".to_string(), s as usize,unsafe {strlen(s)},  Backtrace::new())));
        }
        unsafe {
            atol(s)
        }
    }

    #[inline]
    fn hook_atoll(&mut self, s: *const c_char) -> i64 {
        extern "C" {
            fn atoll(s: *const c_char) -> i64;
            fn strlen(s: *const c_char) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s as *const c_void, unsafe { strlen(s) }) {
            self.report_error(AsanError::BadFuncArgRead(("atoll".to_string(), s as usize, unsafe {strlen(s)}, Backtrace::new())));
        }
        unsafe {
            atoll(s)
        }
    }

    #[inline]
    fn hook_wcslen(&mut self, s: *const wchar_t) -> usize {
        extern "C" {
            fn wcslen(s: *const wchar_t) -> usize;
        }
        let size = unsafe { wcslen(s) };
        if !(self.shadow_check_func.unwrap())(s as *const c_void, (size + 1) * 2) {
            self.report_error(AsanError::BadFuncArgRead(("wcslen".to_string(), s as usize, (size + 1) * 2, Backtrace::new())));
        }
        size
    }

    #[inline]
    fn hook_wcscpy(&mut self, dest: *mut wchar_t, src: *const wchar_t) -> *mut wchar_t {
        extern "C" {
            fn wcscpy(dest: *mut wchar_t, src: *const wchar_t) -> *mut wchar_t;
            fn wcslen(s: *const wchar_t) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(dest as *const c_void, unsafe { (wcslen(src) + 1) * 2 }) {
            self.report_error(AsanError::BadFuncArgWrite(("wcscpy".to_string(), dest as usize,(unsafe {wcslen(src)} + 1) * 2, Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(src as *const c_void, unsafe { (wcslen(src) + 1) * 2 }) {
            self.report_error(AsanError::BadFuncArgRead(("wcscpy".to_string(), src as usize, (unsafe {wcslen(src)} + 1) * 2, Backtrace::new())));
        }
        unsafe {
            wcscpy(dest, src)
        }
    }

    #[inline]
    fn hook_wcscmp(&mut self, s1: *const wchar_t, s2: *const wchar_t) -> i32 {
        extern "C" {
            fn wcscmp(s1: *const wchar_t, s2: *const wchar_t) -> i32;
            fn wcslen(s: *const wchar_t) -> usize;
        }
        if !(self.shadow_check_func.unwrap())(s1 as *const c_void, unsafe { (wcslen(s1) + 1) * 2 }) {
            self.report_error(AsanError::BadFuncArgRead(("wcscmp".to_string(), s1 as usize, (unsafe {wcslen(s1)} +1) * 2,  Backtrace::new())));
        }
        if !(self.shadow_check_func.unwrap())(s2 as *const c_void, unsafe { (wcslen(s2) + 1) * 2 }) {
            self.report_error(AsanError::BadFuncArgRead(("wcscmp".to_string(), s2 as usize, (unsafe {wcslen(s2)} + 1) * 2, Backtrace::new())));
        }
        unsafe {
            wcscmp(s1, s2)
        }
    }

    /// Hook all functions required for ASAN to function, replacing them with our own
    /// implementations.
    fn hook_functions(&mut self, gum: &Gum) {
        let mut interceptor = frida_gum::interceptor::Interceptor::obtain(gum);

        macro_rules! hook_func {
            ($lib:expr, $name:ident, ($($param:ident : $param_type:ty),*), $return_type:ty) => {
                paste::paste! {
                    extern "C" {
                        fn $name($($param: $param_type),*) -> $return_type;
                    }
                    #[allow(non_snake_case)]
                    unsafe extern "C" fn [<replacement_ $name>]($($param: $param_type),*) -> $return_type {
                        let mut invocation = Interceptor::current_invocation();
                        let this = &mut *(invocation.replacement_data().unwrap().0 as *mut AsanRuntime);
                        if this.module_map.as_ref().unwrap().find(invocation.return_addr() as u64).is_some() {
                            this.[<hook_ $name>]($($param),*)
                        } else {
                            $name($($param),*)
                        }
                    }
                    interceptor.replace(
                        frida_gum::Module::find_export_by_name($lib, stringify!($name)).expect("Failed to find function"),
                        NativePointer([<replacement_ $name>] as *mut c_void),
                        NativePointer(self as *mut _ as *mut c_void)
                    ).ok();
                }
            }
        }

        // Hook the memory allocator functions
        hook_func!(None, malloc, (size: usize), *mut c_void);
        hook_func!(None, calloc, (nmemb: usize, size: usize), *mut c_void);
        hook_func!(None, realloc, (ptr: *mut c_void, size: usize), *mut c_void);
        hook_func!(None, free, (ptr: *mut c_void), ());
        hook_func!(None, memalign, (size: usize, alignment: usize), *mut c_void);
        hook_func!(
            None,
            posix_memalign,
            (pptr: *mut *mut c_void, size: usize, alignment: usize),
            i32
        );
        hook_func!(None, malloc_usable_size, (ptr: *mut c_void), usize);
        hook_func!(None, _Znam, (size: usize), *mut c_void);
        hook_func!(
            None,
            _ZnamRKSt9nothrow_t,
            (size: usize, _nothrow: *const c_void),
            *mut c_void
        );
        hook_func!(
            None,
            _ZnamSt11align_val_t,
            (size: usize, alignment: usize),
            *mut c_void
        );
        hook_func!(
            None,
            _ZnamSt11align_val_tRKSt9nothrow_t,
            (size: usize, alignment: usize, _nothrow: *const c_void),
            *mut c_void
        );
        hook_func!(None, _Znwm, (size: usize), *mut c_void);
        hook_func!(
            None,
            _ZnwmRKSt9nothrow_t,
            (size: usize, _nothrow: *const c_void),
            *mut c_void
        );
        hook_func!(
            None,
            _ZnwmSt11align_val_t,
            (size: usize, alignment: usize),
            *mut c_void
        );
        hook_func!(
            None,
            _ZnwmSt11align_val_tRKSt9nothrow_t,
            (size: usize, alignment: usize, _nothrow: *const c_void),
            *mut c_void
        );
        hook_func!(None, _ZdaPv, (ptr: *mut c_void), ());
        hook_func!(None, _ZdaPvm, (ptr: *mut c_void, _ulong: u64), ());
        hook_func!(
            None,
            _ZdaPvmSt11align_val_t,
            (ptr: *mut c_void, _ulong: u64, _alignment: usize),
            ()
        );
        hook_func!(
            None,
            _ZdaPvRKSt9nothrow_t,
            (ptr: *mut c_void, _nothrow: *const c_void),
            ()
        );
        hook_func!(
            None,
            _ZdaPvSt11align_val_t,
            (ptr: *mut c_void, _alignment: usize),
            ()
        );
        hook_func!(
            None,
            _ZdaPvSt11align_val_tRKSt9nothrow_t,
            (ptr: *mut c_void, _alignment: usize, _nothrow: *const c_void),
            ()
        );
        hook_func!(None, _ZdlPv, (ptr: *mut c_void), ());
        hook_func!(None, _ZdlPvm, (ptr: *mut c_void, _ulong: u64), ());
        hook_func!(
            None,
            _ZdlPvmSt11align_val_t,
            (ptr: *mut c_void, _ulong: u64, _alignment: usize),
            ()
        );
        hook_func!(
            None,
            _ZdlPvRKSt9nothrow_t,
            (ptr: *mut c_void, _nothrow: *const c_void),
            ()
        );
        hook_func!(
            None,
            _ZdlPvSt11align_val_t,
            (ptr: *mut c_void, _alignment: usize),
            ()
        );
        hook_func!(
            None,
            _ZdlPvSt11align_val_tRKSt9nothrow_t,
            (ptr: *mut c_void, _alignment: usize, _nothrow: *const c_void),
            ()
        );


        hook_func!(
            None,
            mmap,
            (addr: *const c_void, length: usize, prot: i32, flags: i32, fd: i32, offset: usize),
            *mut c_void
        );
        hook_func!(
            None,
            munmap,
            (addr: *const c_void, length: usize),
            i32
        );

        // Hook libc functions which may access allocated memory
        hook_func!(
            None,
            write,
            (fd: i32, buf: *const c_void, count: usize),
            usize
        );
        hook_func!(
            None,
            read,
            (fd: i32, buf: *mut c_void, count: usize),
            usize
        );
        hook_func!(
            None,
            fgets,
            (s: *mut c_void, size: u32, stream: *mut c_void),
            *mut c_void
        );
        hook_func!(
            None,
            memcmp,
            (s1: *const c_void, s2: *const c_void, n: usize),
            i32
        );
        hook_func!(
            None,
            memcpy,
            (dest: *mut c_void, src: *const c_void, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            mempcpy,
            (dest: *mut c_void, src: *const c_void, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            memmove,
            (dest: *mut c_void, src: *const c_void, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            memset,
            (s: *mut c_void, c: i32, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            memchr,
            (s: *mut c_void, c: i32, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            memrchr,
            (s: *mut c_void, c: i32, n: usize),
            *mut c_void
        );
        hook_func!(
            None,
            memmem,
            (haystack: *const c_void, haystacklen: usize, needle: *const c_void, needlelen: usize),
            *mut c_void
        );
        #[cfg(not(target_os = "android"))]
        hook_func!(
            None,
            bzero,
            (s: *mut c_void, n: usize),
            ()
        );
        #[cfg(not(target_os = "android"))]
        hook_func!(
            None,
            explicit_bzero,
            (s: *mut c_void, n: usize),
            ()
        );
        #[cfg(not(target_os = "android"))]
        hook_func!(
            None,
            bcmp,
            (s1: *const c_void, s2: *const c_void, n: usize),
            i32
        );
        hook_func!(
            None,
            strchr,
            (s: *mut c_char, c: i32),
            *mut c_char
        );
        hook_func!(
            None,
            strrchr,
            (s: *mut c_char, c: i32),
            *mut c_char
        );
        hook_func!(
            None,
            strcasecmp,
            (s1: *const c_char, s2: *const c_char),
            i32
        );
        hook_func!(
            None,
            strncasecmp,
            (s1: *const c_char, s2: *const c_char, n: usize),
            i32
        );
        hook_func!(
            None,
            strcat,
            (dest: *mut c_char, src: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            strcmp,
            (s1: *const c_char, s2: *const c_char),
            i32
        );
        hook_func!(
            None,
            strncmp,
            (s1: *const c_char, s2: *const c_char, n: usize),
            i32
        );
        hook_func!(
            None,
            strcpy,
            (dest: *mut c_char, src: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            strncpy,
            (dest: *mut c_char, src: *const c_char, n: usize),
            *mut c_char
        );
        hook_func!(
            None,
            stpcpy,
            (dest: *mut c_char, src: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            strdup,
            (s: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            strlen,
            (s: *const c_char),
            usize
        );
        hook_func!(
            None,
            strnlen,
            (s: *const c_char, n: usize),
            usize
        );
        hook_func!(
            None,
            strstr,
            (haystack: *const c_char, needle: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            strcasestr,
            (haystack: *const c_char, needle: *const c_char),
            *mut c_char
        );
        hook_func!(
            None,
            atoi,
            (nptr: *const c_char),
            i32
        );
        hook_func!(
            None,
            atol,
            (nptr: *const c_char),
            i32
        );
        hook_func!(
            None,
            atoll,
            (nptr: *const c_char),
            i64
        );
        hook_func!(
            None,
            wcslen,
            (s: *const wchar_t),
            usize
        );
        hook_func!(
            None,
            wcscpy,
            (dest: *mut wchar_t, src: *const wchar_t),
            *mut wchar_t
        );
        hook_func!(
            None,
            wcscmp,
            (s1: *const wchar_t, s2: *const wchar_t),
            i32
        );

    }

    #[allow(clippy::cast_sign_loss)] // for displacement
    #[allow(clippy::too_many_lines)]
    extern "C" fn handle_trap(&mut self) {
        let mut actual_pc = self.regs[31];
        actual_pc = match self.stalked_addresses.get(&actual_pc) {
            Some(addr) => *addr,
            None => actual_pc,
        };

        let cs = Capstone::new()
            .arm64()
            .mode(capstone::arch::arm64::ArchMode::Arm)
            .detail(true)
            .build()
            .unwrap();

        let instructions = cs
            .disasm_count(
                unsafe { std::slice::from_raw_parts(actual_pc as *mut u8, 24) },
                actual_pc as u64,
                3,
            )
            .unwrap();
        let instructions = instructions.iter().collect::<Vec<Insn>>();
        let mut insn = instructions.first().unwrap();
        if insn.mnemonic().unwrap() == "msr" && insn.op_str().unwrap() == "nzcv, x0" {
            insn = instructions.get(2).unwrap();
            actual_pc = insn.address() as usize;
        }

        let detail = cs.insn_detail(&insn).unwrap();
        let arch_detail = detail.arch_detail();
        let (mut base_reg, mut index_reg, displacement) =
            if let Arm64Operand(arm64operand) = arch_detail.operands().last().unwrap() {
                if let Arm64OperandType::Mem(opmem) = arm64operand.op_type {
                    (opmem.base().0, opmem.index().0, opmem.disp())
                } else {
                    (0, 0, 0)
                }
            } else {
                (0, 0, 0)
            };

        if capstone::arch::arm64::Arm64Reg::ARM64_REG_X0 as u16 <= base_reg
            && base_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_X28 as u16
        {
            base_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_X0 as u16;
        } else if base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_X29 as u16 {
            base_reg = 29u16;
        } else if base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_X30 as u16 {
            base_reg = 30u16;
        } else if base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_SP as u16
            || base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_WSP as u16
            || base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_XZR as u16
            || base_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_WZR as u16
        {
            base_reg = 31u16;
        } else if capstone::arch::arm64::Arm64Reg::ARM64_REG_W0 as u16 <= base_reg
            && base_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_W30 as u16
        {
            base_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_W0 as u16;
        } else if capstone::arch::arm64::Arm64Reg::ARM64_REG_S0 as u16 <= base_reg
            && base_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_S31 as u16
        {
            base_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_S0 as u16;
        }

        #[allow(clippy::clippy::cast_possible_wrap)]
        let mut fault_address =
            (self.regs[base_reg as usize] as isize + displacement as isize) as usize;

        if index_reg == 0 {
            index_reg = 0xffff
        } else {
            if capstone::arch::arm64::Arm64Reg::ARM64_REG_X0 as u16 <= index_reg
                && index_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_X28 as u16
            {
                index_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_X0 as u16;
            } else if index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_X29 as u16 {
                index_reg = 29u16;
            } else if index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_X30 as u16 {
                index_reg = 30u16;
            } else if index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_SP as u16
                || index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_WSP as u16
                || index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_XZR as u16
                || index_reg == capstone::arch::arm64::Arm64Reg::ARM64_REG_WZR as u16
            {
                index_reg = 31u16;
            } else if capstone::arch::arm64::Arm64Reg::ARM64_REG_W0 as u16 <= index_reg
                && index_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_W30 as u16
            {
                index_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_W0 as u16;
            } else if capstone::arch::arm64::Arm64Reg::ARM64_REG_S0 as u16 <= index_reg
                && index_reg <= capstone::arch::arm64::Arm64Reg::ARM64_REG_S31 as u16
            {
                index_reg -= capstone::arch::arm64::Arm64Reg::ARM64_REG_S0 as u16;
            }
            fault_address += self.regs[index_reg as usize] as usize;
        }

        let backtrace = Backtrace::new();

        let (stack_start, stack_end) = Self::current_stack();
        let error = if fault_address >= stack_start && fault_address < stack_end {
            if insn.mnemonic().unwrap().starts_with('l') {
                AsanError::StackOobRead((
                    self.regs,
                    actual_pc,
                    (base_reg, index_reg, displacement as usize, fault_address),
                    backtrace,
                ))
            } else {
                AsanError::StackOobWrite((
                    self.regs,
                    actual_pc,
                    (base_reg, index_reg, displacement as usize, fault_address),
                    backtrace,
                ))
            }
        } else {
            let mut allocator = Allocator::get();
            #[allow(clippy::option_if_let_else)]
            if let Some(metadata) =
                allocator.find_metadata(fault_address, self.regs[base_reg as usize])
            {
                let asan_readwrite_error = AsanReadWriteError {
                    registers: self.regs,
                    pc: actual_pc,
                    fault: (base_reg, index_reg, displacement as usize, fault_address),
                    metadata: metadata.clone(),
                    backtrace,
                };
                if insn.mnemonic().unwrap().starts_with('l') {
                    if metadata.freed {
                        AsanError::ReadAfterFree(asan_readwrite_error)
                    } else {
                        AsanError::OobRead(asan_readwrite_error)
                    }
                } else if metadata.freed {
                    AsanError::WriteAfterFree(asan_readwrite_error)
                } else {
                    AsanError::OobWrite(asan_readwrite_error)
                }
            } else {
                AsanError::Unknown((
                    self.regs,
                    actual_pc,
                    (base_reg, index_reg, displacement as usize, fault_address),
                    backtrace,
                ))
            }
        };
        self.report_error(error);
    }

    #[allow(clippy::too_many_lines)]
    fn report_error(&mut self, error: AsanError) {
        unsafe {
            ASAN_ERRORS.as_mut().unwrap().errors.push(error.clone());
        }

        let mut out_stream = default_output_stream();
        let output = out_stream.as_mut();

        let backtrace_printer = BacktracePrinter::new()
            .clear_frame_filters()
            .print_addresses(true)
            .verbosity(Verbosity::Full)
            .add_frame_filter(Box::new(|frames| {
                frames.retain(
                    |x| matches!(&x.name, Some(n) if !n.starts_with("libafl_frida::asan_rt::")),
                )
            }));

        #[allow(clippy::non_ascii_literal)]
        writeln!(output, "{:━^100}", " Memory error detected! ").unwrap();
        output
            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
            .unwrap();
        write!(output, "{}", error.description()).unwrap();
        match error {
            AsanError::OobRead(mut error)
            | AsanError::OobWrite(mut error)
            | AsanError::ReadAfterFree(mut error)
            | AsanError::WriteAfterFree(mut error) => {
                let (basereg, indexreg, _displacement, fault_address) = error.fault;

                if let Some((range, path)) = self.instrumented_ranges.get_key_value(&error.pc) {
                    writeln!(
                        output,
                        " at 0x{:x} ({}@0x{:04x}), faulting address 0x{:x}",
                        error.pc,
                        path,
                        error.pc - range.start,
                        fault_address
                    )
                    .unwrap();
                } else {
                    writeln!(
                        output,
                        " at 0x{:x}, faulting address 0x{:x}",
                        error.pc, fault_address
                    )
                    .unwrap();
                }
                output.reset().unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " REGISTERS ").unwrap();
                for reg in 0..=30 {
                    if reg == basereg {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                            .unwrap();
                    } else if reg == indexreg {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Yellow)))
                            .unwrap();
                    }
                    write!(
                        output,
                        "x{:02}: 0x{:016x} ",
                        reg, error.registers[reg as usize]
                    )
                    .unwrap();
                    output.reset().unwrap();
                    if reg % 4 == 3 {
                        writeln!(output).unwrap();
                    }
                }
                writeln!(output, "pc : 0x{:016x} ", error.pc).unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " CODE ").unwrap();
                let mut cs = Capstone::new()
                    .arm64()
                    .mode(capstone::arch::arm64::ArchMode::Arm)
                    .build()
                    .unwrap();
                cs.set_skipdata(true).expect("failed to set skipdata");

                let start_pc = error.pc - 4 * 5;
                for insn in cs
                    .disasm_count(
                        unsafe { std::slice::from_raw_parts(start_pc as *mut u8, 4 * 11) },
                        start_pc as u64,
                        11,
                    )
                    .expect("failed to disassemble instructions")
                    .iter()
                {
                    if insn.address() as usize == error.pc {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                            .unwrap();
                        writeln!(output, "\t => {}", insn).unwrap();
                        output.reset().unwrap();
                    } else {
                        writeln!(output, "\t    {}", insn).unwrap();
                    }
                }
                backtrace_printer
                    .print_trace(&error.backtrace, output)
                    .unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " ALLOCATION INFO ").unwrap();
                let offset: i64 = fault_address as i64 - error.metadata.address as i64;
                let direction = if offset > 0 { "right" } else { "left" };
                writeln!(
                    output,
                    "access is {} to the {} of the 0x{:x} byte allocation at 0x{:x}",
                    offset, direction, error.metadata.size, error.metadata.address
                )
                .unwrap();

                if error.metadata.is_malloc_zero {
                    writeln!(output, "allocation was zero-sized").unwrap();
                }

                if let Some(backtrace) = error.metadata.allocation_site_backtrace.as_mut() {
                    writeln!(output, "allocation site backtrace:").unwrap();
                    backtrace.resolve();
                    backtrace_printer.print_trace(backtrace, output).unwrap();
                }

                if error.metadata.freed {
                    #[allow(clippy::non_ascii_literal)]
                    writeln!(output, "{:━^100}", " FREE INFO ").unwrap();
                    if let Some(backtrace) = error.metadata.release_site_backtrace.as_mut() {
                        writeln!(output, "free site backtrace:").unwrap();
                        backtrace.resolve();
                        backtrace_printer.print_trace(backtrace, output).unwrap();
                    }
                }
            }
            AsanError::BadFuncArgRead((name, address, size, backtrace)) | AsanError::BadFuncArgWrite((name, address, size, backtrace)) => {
                writeln!(output, " in call to {}, argument {:#016x}, size: {:#x}", name, address, size).unwrap();
                let invocation = Interceptor::current_invocation();
                let cpu_context = invocation.cpu_context();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " REGISTERS ").unwrap();
                for reg in 0..29 {
                    let val = cpu_context.reg(reg);
                    if val as usize == address {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                            .unwrap();
                    }
                    write!(
                        output,
                        "x{:02}: 0x{:016x} ",
                        reg, val
                    )
                    .unwrap();
                    output.reset().unwrap();
                    if reg % 4 == 3 {
                        writeln!(output).unwrap();
                    }
                }
                write!(output, "sp : 0x{:016x} ", cpu_context.sp()).unwrap();
                write!(output, "lr : 0x{:016x} ", cpu_context.lr()).unwrap();
                writeln!(output, "pc : 0x{:016x} ", cpu_context.pc()).unwrap();

                backtrace_printer
                    .print_trace(&backtrace, output)
                    .unwrap();

            }
            AsanError::DoubleFree((ptr, mut metadata, backtrace)) => {
                writeln!(output, " of {:?}", ptr).unwrap();
                output.reset().unwrap();
                backtrace_printer.print_trace(&backtrace, output).unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " ALLOCATION INFO ").unwrap();
                writeln!(
                    output,
                    "allocation at 0x{:x}, with size 0x{:x}",
                    metadata.address, metadata.size
                )
                .unwrap();
                if metadata.is_malloc_zero {
                    writeln!(output, "allocation was zero-sized").unwrap();
                }

                if let Some(backtrace) = metadata.allocation_site_backtrace.as_mut() {
                    writeln!(output, "allocation site backtrace:").unwrap();
                    backtrace.resolve();
                    backtrace_printer.print_trace(backtrace, output).unwrap();
                }
                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " FREE INFO ").unwrap();
                if let Some(backtrace) = metadata.release_site_backtrace.as_mut() {
                    writeln!(output, "previous free site backtrace:").unwrap();
                    backtrace.resolve();
                    backtrace_printer.print_trace(backtrace, output).unwrap();
                }
            }
            AsanError::UnallocatedFree((ptr, backtrace)) => {
                writeln!(output, " of {:#016x}", ptr).unwrap();
                output.reset().unwrap();
                backtrace_printer.print_trace(&backtrace, output).unwrap();
            }
            AsanError::Leak((ptr, mut metadata)) => {
                writeln!(output, " of {:#016x}", ptr).unwrap();
                output.reset().unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " ALLOCATION INFO ").unwrap();
                writeln!(
                    output,
                    "allocation at 0x{:x}, with size 0x{:x}",
                    metadata.address, metadata.size
                )
                .unwrap();
                if metadata.is_malloc_zero {
                    writeln!(output, "allocation was zero-sized").unwrap();
                }

                if let Some(backtrace) = metadata.allocation_site_backtrace.as_mut() {
                    writeln!(output, "allocation site backtrace:").unwrap();
                    backtrace.resolve();
                    backtrace_printer.print_trace(backtrace, output).unwrap();
                }
            }
            AsanError::Unknown((registers, pc, fault, backtrace))
            | AsanError::StackOobRead((registers, pc, fault, backtrace))
            | AsanError::StackOobWrite((registers, pc, fault, backtrace)) => {
                let (basereg, indexreg, _displacement, fault_address) = fault;

                if let Ok((start, _, _, path)) = find_mapping_for_address(pc) {
                    writeln!(
                        output,
                        " at 0x{:x} ({}:0x{:04x}), faulting address 0x{:x}",
                        pc,
                        path,
                        pc - start,
                        fault_address
                    )
                    .unwrap();
                } else {
                    writeln!(
                        output,
                        " at 0x{:x}, faulting address 0x{:x}",
                        pc, fault_address
                    )
                    .unwrap();
                }
                output.reset().unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " REGISTERS ").unwrap();
                for reg in 0..=30 {
                    if reg == basereg {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                            .unwrap();
                    } else if reg == indexreg {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Yellow)))
                            .unwrap();
                    }
                    write!(output, "x{:02}: 0x{:016x} ", reg, registers[reg as usize]).unwrap();
                    output.reset().unwrap();
                    if reg % 4 == 3 {
                        writeln!(output).unwrap();
                    }
                }
                writeln!(output, "pc : 0x{:016x} ", pc).unwrap();

                #[allow(clippy::non_ascii_literal)]
                writeln!(output, "{:━^100}", " CODE ").unwrap();
                let mut cs = Capstone::new()
                    .arm64()
                    .mode(capstone::arch::arm64::ArchMode::Arm)
                    .build()
                    .unwrap();
                cs.set_skipdata(true).expect("failed to set skipdata");

                let start_pc = pc - 4 * 5;
                for insn in cs
                    .disasm_count(
                        unsafe { std::slice::from_raw_parts(start_pc as *mut u8, 4 * 11) },
                        start_pc as u64,
                        11,
                    )
                    .expect("failed to disassemble instructions")
                    .iter()
                {
                    if insn.address() as usize == pc {
                        output
                            .set_color(ColorSpec::new().set_fg(Some(Color::Red)))
                            .unwrap();
                        writeln!(output, "\t => {}", insn).unwrap();
                        output.reset().unwrap();
                    } else {
                        writeln!(output, "\t    {}", insn).unwrap();
                    }
                }
                backtrace_printer.print_trace(&backtrace, output).unwrap();
            }
        };

        if !self.options.asan_continue_after_error() {
            panic!("Crashing target!");
        }
    }

    #[allow(clippy::unused_self)]
    fn generate_shadow_check_function(&mut self) {
        let shadow_bit = Allocator::get().shadow_bit as u32;
        let mut ops = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        dynasm!(ops
            ; .arch aarch64

            // calculate the shadow address
            ; mov x5, #1
            ; add x5, xzr, x5, lsl #shadow_bit
            ; add x5, x5, x0, lsr #3
            ; ubfx x5, x5, #0, #(shadow_bit + 2)

            ; cmp x1, #0
            ; b.eq >return_success
            // check if the ptr is not aligned to 8 bytes
            ; ands x6, x0, #7
            ; b.eq >no_start_offset

            // we need to test the high bits from the first shadow byte
            ; ldrh w7, [x5, #0]
            ; rev16 w7, w7
            ; rbit w7, w7
            ; lsr x7, x7, #16
            ; lsr x7, x7, x6

            ; cmp x1, #8
            ; b.lt >dont_fill_to_8
            ; mov x2, #8
            ; sub x6, x2, x6
            ; b >check_bits
            ; dont_fill_to_8:
            ; mov x6, x1
            ; check_bits:
            ; mov x2, #1
            ; lsl x2, x2, x6
            ; sub x4, x2, #1

            // if shadow_bits & size_to_test != size_to_test: fail
            ; and x7, x7, x4
            ; cmp x7, x4
            ; b.ne >return_failure

            // size -= size_to_test
            ; sub x1, x1, x6
            // shadow_addr += 1 (we consumed the initial byte in the above test)
            ; add x5, x5, 1

            ; no_start_offset:
            // num_shadow_bytes = size / 8
            ; lsr x4, x1, #3
            ; eor x3, x3, x3
            ; sub x3, x3, #1

            // if num_shadow_bytes < 8; then goto check_bytes; else check_8_shadow_bytes
            ; check_8_shadow_bytes:
            ; cmp x4, #0x8
            ; b.lt >less_than_8_shadow_bytes_remaining
            ; ldr x7, [x5], #8
            ; cmp x7, x3
            ; b.ne >return_failure
            ; sub x4, x4, #8
            ; sub x1, x1, #64
            ; b <check_8_shadow_bytes

            ; less_than_8_shadow_bytes_remaining:
            ; cmp x4, #1
            ; b.lt >check_trailing_bits
            ; ldrb w7, [x5], #1
            ; cmp w7, #0xff
            ; b.ne >return_failure
            ; sub x4, x4, #1
            ; sub x1, x1, #8
            ; b <less_than_8_shadow_bytes_remaining

            ; check_trailing_bits:
            ; cmp x1, #0x0
            ; b.eq >return_success

            ; and x4, x1, #7
            ; mov x2, #1
            ; lsl x2, x2, x4
            ; sub x4, x2, #1

            ; ldrh w7, [x5, #0]
            ; rev16 w7, w7
            ; rbit w7, w7
            ; lsr x7, x7, #16
            ; and x7, x7, x4
            ; cmp x7, x4
            ; b.ne >return_failure

            ; return_success:
            ; mov x0, #1
            ; b >prologue

            ; return_failure:
            ; mov x0, #0


            ; prologue:
            ; ret
        );

        let blob = ops.finalize().unwrap();
        unsafe {
            let mapping = mmap(std::ptr::null_mut(), 0x1000, ProtFlags::all(), MapFlags::MAP_ANON | MapFlags::MAP_PRIVATE, -1, 0).unwrap();
            blob.as_ptr().copy_to_nonoverlapping(mapping as *mut u8, blob.len());
            self.shadow_check_func = Some(std::mem::transmute(mapping as *mut u8));
        }
    }

    #[allow(clippy::unused_self)]
    fn generate_shadow_check_blob(&mut self, bit: u32) -> Box<[u8]> {
        let shadow_bit = Allocator::get().shadow_bit as u32;
        macro_rules! shadow_check {
            ($ops:ident, $bit:expr) => {dynasm!($ops
                ; .arch aarch64

                ; mov x1, #1
                ; add x1, xzr, x1, lsl #shadow_bit
                ; add x1, x1, x0, lsr #3
                ; ubfx x1, x1, #0, #(shadow_bit + 2)
                ; ldrh w1, [x1, #0]
                ; and x0, x0, #7
                ; rev16 w1, w1
                ; rbit w1, w1
                ; lsr x1, x1, #16
                ; lsr x1, x1, x0
                ; tbnz x1, #$bit, >done

                ; adr x1, >done
                ; nop // will be replaced by b to report
                ; done:
            );};
        }

        let mut ops = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check!(ops, bit);
        let ops_vec = ops.finalize().unwrap();
        ops_vec[..ops_vec.len() - 4].to_vec().into_boxed_slice()
    }

    #[allow(clippy::unused_self)]
    fn generate_shadow_check_exact_blob(&mut self, val: u64) -> Box<[u8]> {
        let shadow_bit = Allocator::get().shadow_bit as u32;
        macro_rules! shadow_check_exact {
            ($ops:ident, $val:expr) => {dynasm!($ops
                ; .arch aarch64

                ; mov x1, #1
                ; add x1, xzr, x1, lsl #shadow_bit
                ; add x1, x1, x0, lsr #3
                ; ubfx x1, x1, #0, #(shadow_bit + 2)
                ; ldrh w1, [x1, #0]
                ; and x0, x0, #7
                ; rev16 w1, w1
                ; rbit w1, w1
                ; lsr x1, x1, #16
                ; lsr x1, x1, x0
                ; .dword -717536768 // 0xd53b4200 //mrs x0, NZCV
                ; stp x2, x3, [sp, #-0x10]!
                ; mov x2, $val
                ; ands x1, x1, x2
                ; ldp x2, x3, [sp], 0x10
                ; b.ne >done

                ; adr x1, >done
                ; nop // will be replaced by b to report
                ; done:
            );};
        }

        let mut ops = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        shadow_check_exact!(ops, val);
        let ops_vec = ops.finalize().unwrap();
        ops_vec[..ops_vec.len() - 4].to_vec().into_boxed_slice()
    }

    ///
    /// Generate the instrumentation blobs for the current arch.
    #[allow(clippy::similar_names)] // We allow things like dword and qword
    #[allow(clippy::cast_possible_wrap)]
    #[allow(clippy::too_many_lines)]
    fn generate_instrumentation_blobs(&mut self) {
        let mut ops_report = dynasmrt::VecAssembler::<dynasmrt::aarch64::Aarch64Relocation>::new(0);
        dynasm!(ops_report
            ; .arch aarch64

            ; report:
            ; stp x29, x30, [sp, #-0x10]!
            ; mov x29, sp
            // save the nvcz and the 'return-address'/address of instrumented instruction
            ; stp x0, x1, [sp, #-0x10]!

            ; ldr x0, >self_regs_addr
            ; stp x2, x3, [x0, #0x10]
            ; stp x4, x5, [x0, #0x20]
            ; stp x6, x7, [x0, #0x30]
            ; stp x8, x9, [x0, #0x40]
            ; stp x10, x11, [x0, #0x50]
            ; stp x12, x13, [x0, #0x60]
            ; stp x14, x15, [x0, #0x70]
            ; stp x16, x17, [x0, #0x80]
            ; stp x18, x19, [x0, #0x90]
            ; stp x20, x21, [x0, #0xa0]
            ; stp x22, x23, [x0, #0xb0]
            ; stp x24, x25, [x0, #0xc0]
            ; stp x26, x27, [x0, #0xd0]
            ; stp x28, x29, [x0, #0xe0]
            ; stp x30, xzr, [x0, #0xf0]
            ; mov x28, x0

            ; mov x25, x1 // address of instrumented instruction.
            ; str x25, [x28, 0xf8]

            ; .dword 0xd53b4218u32 as i32 // mrs x24, nzcv
            ; ldp x0, x1, [sp, 0x20]
            ; stp x0, x1, [x28]

            ; adr x25, <report
            ; adr x0, >eh_frame_fde
            ; adr x27, >fde_address
            ; ldr w26, [x27]
            ; cmp w26, #0x0
            ; b.ne >skip_register
            ; sub x25, x25, x27
            ; str w25, [x27]
            ; ldr x1, >register_frame_func
            //; brk #11
            ; blr x1
            ; skip_register:
            ; ldr x0, >self_addr
            ; ldr x1, >trap_func
            ; blr x1

            ; .dword 0xd51b4218u32 as i32 // msr nzcv, x24
            ; ldr x0, >self_regs_addr
            ; ldp x2, x3, [x0, #0x10]
            ; ldp x4, x5, [x0, #0x20]
            ; ldp x6, x7, [x0, #0x30]
            ; ldp x8, x9, [x0, #0x40]
            ; ldp x10, x11, [x0, #0x50]
            ; ldp x12, x13, [x0, #0x60]
            ; ldp x14, x15, [x0, #0x70]
            ; ldp x16, x17, [x0, #0x80]
            ; ldp x18, x19, [x0, #0x90]
            ; ldp x20, x21, [x0, #0xa0]
            ; ldp x22, x23, [x0, #0xb0]
            ; ldp x24, x25, [x0, #0xc0]
            ; ldp x26, x27, [x0, #0xd0]
            ; ldp x28, x29, [x0, #0xe0]
            ; ldp x30, xzr, [x0, #0xf0]

            // restore nzcv. and 'return address'
            ; ldp x0, x1, [sp], #0x10
            ; ldp x29, x30, [sp], #0x10
            ; br x1 // go back to the 'return address'

            ; self_addr:
            ; .qword self as *mut _  as *mut c_void as i64
            ; self_regs_addr:
            ; .qword &mut self.regs as *mut _ as *mut c_void as i64
            ; trap_func:
            ; .qword AsanRuntime::handle_trap as *mut c_void as i64
            ; register_frame_func:
            ; .qword __register_frame as *mut c_void as i64
            ; eh_frame_cie:
            ; .dword 0x14
            ; .dword 0x00
            ; .dword 0x00527a01
            ; .dword 0x011e7c01
            ; .dword 0x001f0c1b
            ; eh_frame_fde:
            ; .dword 0x14
            ; .dword 0x18
            ; fde_address:
            ; .dword 0x0 // <-- address offset goes here
            ; .dword 0x104
                //advance_loc 12
                //def_cfa r29 (x29) at offset 16
                //offset r30 (x30) at cfa-8
                //offset r29 (x29) at cfa-16
            ; .dword 0x1d0c4c00
            ; .dword 0x9d029e10u32 as i32
            ; .dword 0x04
            // empty next FDE:
            ; .dword 0x0
            ; .dword 0x0
        );
        self.blob_report = Some(ops_report.finalize().unwrap().into_boxed_slice());

        self.blob_check_mem_byte = Some(self.generate_shadow_check_blob(0));
        self.blob_check_mem_halfword = Some(self.generate_shadow_check_blob(1));
        self.blob_check_mem_dword = Some(self.generate_shadow_check_blob(2));
        self.blob_check_mem_qword = Some(self.generate_shadow_check_blob(3));
        self.blob_check_mem_16bytes = Some(self.generate_shadow_check_blob(4));

        self.blob_check_mem_3bytes = Some(self.generate_shadow_check_exact_blob(3));
        self.blob_check_mem_6bytes = Some(self.generate_shadow_check_exact_blob(6));
        self.blob_check_mem_12bytes = Some(self.generate_shadow_check_exact_blob(12));
        self.blob_check_mem_24bytes = Some(self.generate_shadow_check_exact_blob(24));
        self.blob_check_mem_32bytes = Some(self.generate_shadow_check_exact_blob(32));
        self.blob_check_mem_48bytes = Some(self.generate_shadow_check_exact_blob(48));
        self.blob_check_mem_64bytes = Some(self.generate_shadow_check_exact_blob(64));
    }

    /// Get the blob which implements the report funclet
    #[must_use]
    #[inline]
    pub fn blob_report(&self) -> &[u8] {
        self.blob_report.as_ref().unwrap()
    }

    /// Get the blob which checks a byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_byte(&self) -> &[u8] {
        self.blob_check_mem_byte.as_ref().unwrap()
    }

    /// Get the blob which checks a halfword access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_halfword(&self) -> &[u8] {
        self.blob_check_mem_halfword.as_ref().unwrap()
    }

    /// Get the blob which checks a dword access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_dword(&self) -> &[u8] {
        self.blob_check_mem_dword.as_ref().unwrap()
    }

    /// Get the blob which checks a qword access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_qword(&self) -> &[u8] {
        self.blob_check_mem_qword.as_ref().unwrap()
    }

    /// Get the blob which checks a 16 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_16bytes(&self) -> &[u8] {
        self.blob_check_mem_16bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 3 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_3bytes(&self) -> &[u8] {
        self.blob_check_mem_3bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 6 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_6bytes(&self) -> &[u8] {
        self.blob_check_mem_6bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 12 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_12bytes(&self) -> &[u8] {
        self.blob_check_mem_12bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 24 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_24bytes(&self) -> &[u8] {
        self.blob_check_mem_24bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 32 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_32bytes(&self) -> &[u8] {
        self.blob_check_mem_32bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 48 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_48bytes(&self) -> &[u8] {
        self.blob_check_mem_48bytes.as_ref().unwrap()
    }

    /// Get the blob which checks a 64 byte access
    #[must_use]
    #[inline]
    pub fn blob_check_mem_64bytes(&self) -> &[u8] {
        self.blob_check_mem_64bytes.as_ref().unwrap()
    }
}

/// static field for `AsanErrors` for a run
pub static mut ASAN_ERRORS: Option<AsanErrors> = None;

/// An observer for frida address sanitizer `AsanError`s for a frida executor run
#[derive(Serialize, Deserialize)]
#[allow(clippy::unsafe_derive_deserialize)]
pub struct AsanErrorsObserver {
    errors: OwnedPtr<Option<AsanErrors>>,
}

impl Observer for AsanErrorsObserver {}

impl<EM, I, S, Z> HasExecHooks<EM, I, S, Z> for AsanErrorsObserver {
    fn pre_exec(
        &mut self,
        _fuzzer: &mut Z,
        _state: &mut S,
        _mgr: &mut EM,
        _input: &I,
    ) -> Result<(), Error> {
        unsafe {
            if ASAN_ERRORS.is_some() {
                ASAN_ERRORS.as_mut().unwrap().clear();
            }
        }

        Ok(())
    }
}

impl Named for AsanErrorsObserver {
    #[inline]
    fn name(&self) -> &str {
        "AsanErrors"
    }
}

impl AsanErrorsObserver {
    /// Creates a new `AsanErrorsObserver`, pointing to a constant `AsanErrors` field
    #[must_use]
    pub fn new(errors: &'static Option<AsanErrors>) -> Self {
        Self {
            errors: OwnedPtr::Ptr(errors as *const Option<AsanErrors>),
        }
    }

    /// Creates a new `AsanErrorsObserver`, owning the `AsanErrors`
    #[must_use]
    pub fn new_owned(errors: Option<AsanErrors>) -> Self {
        Self {
            errors: OwnedPtr::Owned(Box::new(errors)),
        }
    }

    /// Creates a new `AsanErrorsObserver` from a raw ptr
    #[must_use]
    pub fn new_from_ptr(errors: *const Option<AsanErrors>) -> Self {
        Self {
            errors: OwnedPtr::Ptr(errors),
        }
    }

    /// gets the [`AsanErrors`] from the previous run
    #[must_use]
    pub fn errors(&self) -> Option<&AsanErrors> {
        match &self.errors {
            OwnedPtr::Ptr(p) => unsafe { p.as_ref().unwrap().as_ref() },
            OwnedPtr::Owned(b) => b.as_ref().as_ref(),
        }
    }
}

/// A feedback reporting potential [`AsanErrors`] from an `AsanErrorsObserver`
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct AsanErrorsFeedback {
    errors: Option<AsanErrors>,
}

impl<I, S> Feedback<I, S> for AsanErrorsFeedback
where
    I: Input + HasTargetBytes,
{
    fn is_interesting<EM, OT>(
        &mut self,
        _state: &mut S,
        _manager: &mut EM,
        _input: &I,
        observers: &OT,
        _exit_kind: &ExitKind,
    ) -> Result<bool, Error>
    where
        EM: EventFirer<I, S>,
        OT: ObserversTuple,
    {
        let observer = observers
            .match_name::<AsanErrorsObserver>("AsanErrors")
            .expect("An AsanErrorsFeedback needs an AsanErrorsObserver");
        match observer.errors() {
            None => Ok(false),
            Some(errors) => {
                if errors.errors.is_empty() {
                    Ok(false)
                } else {
                    self.errors = Some(errors.clone());
                    Ok(true)
                }
            }
        }
    }

    fn append_metadata(&mut self, _state: &mut S, testcase: &mut Testcase<I>) -> Result<(), Error> {
        if let Some(errors) = &self.errors {
            testcase.add_metadata(errors.clone());
        }

        Ok(())
    }

    fn discard_metadata(&mut self, _state: &mut S, _input: &I) -> Result<(), Error> {
        self.errors = None;
        Ok(())
    }
}

impl Named for AsanErrorsFeedback {
    #[inline]
    fn name(&self) -> &str {
        "AsanErrors"
    }
}

impl AsanErrorsFeedback {
    /// Create a new `AsanErrorsFeedback`
    #[must_use]
    pub fn new() -> Self {
        Self { errors: None }
    }
}

impl Default for AsanErrorsFeedback {
    fn default() -> Self {
        Self::new()
    }
}
