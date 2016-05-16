use core::isize;

use spin::Mutex;

use c_types::*;
use errno::{set_errno, ENOMEM};
use malloc::expand_heap::__expand_heap;
use mmap::{__mmap, mremap_helper};
use platform::atomic::{a_and_64, a_crash, a_ctz_64, a_store, a_swap};
use platform::malloc::*;
use platform::mman::*;
use thread::{__wait, __wake};

pub const MMAP_THRESHOLD: usize = 0x1c00 * SIZE_ALIGN;
pub const DONTCARE: usize = 16;
pub const RECLAIM: usize = 163_840;

#[repr(C)]
pub struct chunk {
    psize: usize,
    csize: usize,
    next: *mut chunk,
    prev: *mut chunk,
}

#[repr(C)]
pub struct bin {
    lock: [c_int; 2],
    head: *mut chunk,
    tail: *mut chunk,
}

#[repr(C)]
pub struct mal {
    binmap: u64,
    bins: [bin; 64],
    free_lock: [c_int; 2],
}

extern "C" {
    static mut mal: mal;
    fn bin_index(s: usize) -> c_int;
    fn bin_index_up(x: usize) -> c_int;

    fn free(p: *mut c_void);
    fn memcpy(dest: *mut c_void, src: *const c_void, n: usize) -> *mut u8;
}

#[no_mangle]
pub unsafe extern "C" fn malloc(mut n: usize) -> *mut c_void {
    let mut c: *mut chunk;

    if adjust_size(&mut n as *mut usize) < 0 {
        return 0 as *mut c_void;
    }

    if n > MMAP_THRESHOLD {
        let len = n + OVERHEAD + PAGE_SIZE as usize - 1 & (-PAGE_SIZE) as usize;
        let base = __mmap(0 as *mut c_void,
                          len,
                          PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS,
                          -1,
                          0) as *mut u8;

        if base == ((-1isize) as usize) as *mut u8 {
            return 0 as *mut c_void;
        }

        c = base.offset((SIZE_ALIGN - OVERHEAD) as isize) as *mut chunk;
        (*c).csize = len - (SIZE_ALIGN - OVERHEAD);
        (*c).psize = SIZE_ALIGN - OVERHEAD;
        return chunk_to_mem(c);
    }

    let i = bin_index_up(n);
    loop {
        let mask = mal.binmap & (-((1usize << i) as isize)) as u64;
        if mask == 0 {
            c = expand_heap(n);
            if c == 0 as *mut chunk {
                return 0 as *mut c_void;
            }
            if alloc_rev(c) != 0 {
                let x = c;
                c = previous_chunk(c);

                let new = (*x).csize + chunk_size(c);
                (*c).csize = new;
                (*next_chunk(x)).psize = new;
            }
            break;
        }

        let j = a_ctz_64(mask) as c_int;
        lock_bin(j);
        c = mal.bins[j as usize].head;

        if c != bin_to_chunk(j as usize) {
            if pretrim(c, n, i, j) == 0 {
                unbin(c, j);
            }
            unlock_bin(j);
            break;
        }
        unlock_bin(j);
    }

    // Now patch up in case we over-allocated
    trim(c, n);

    chunk_to_mem(c)
}

#[no_mangle]
pub unsafe extern "C" fn __malloc0(n: usize) -> *mut c_void {
    let p = malloc(n);

    if p as usize != 0 && !is_mmapped(mem_to_chunk(p)) {
        for i in 0..n {
            *(p as *mut u8).offset(i as isize) = 0;
        }
    }

    p
}

#[no_mangle]
pub unsafe extern "C" fn realloc(p: *mut c_void, mut n: usize) -> *mut c_void {

    if p as usize == 0 {
        return malloc(n);
    }

    if adjust_size(&mut n) < 0 {
        return 0 as *mut c_void;
    }

    let s = mem_to_chunk(p);
    let n0 = chunk_size(s);

    if is_mmapped(s) {
        let extra = (*s).psize;
        let base = (s as *mut u8).offset(-(extra as isize));
        let old_len = n0 + extra;
        let new_len = n + extra;

        // Crash on realloc of freed chunk
        if extra & 1 != 0 {
            a_crash();
        }

        let new = malloc(n);
        if new_len < PAGE_SIZE as usize && new != 0 as *mut c_void {
            memcpy(new, p, n - OVERHEAD);
            free(p);
            return new;
        }

        let new_len = (new_len + PAGE_SIZE as usize - 1) & (-PAGE_SIZE) as usize;

        if old_len == new_len {
            return p;
        }

        let base = mremap_helper(base as *mut c_void, old_len, new_len, MREMAP_MAYMOVE, None);

        if base as usize == (-1isize) as usize {
            return if new_len < old_len {
                p
            } else {
                0 as *mut c_void
            };
        }

        let s = base.offset(extra as isize) as *mut chunk;
        (*s).csize = new_len - extra;

        return chunk_to_mem(s);
    }

    let mut next = next_chunk(s);

    // Crash on corrupted footer (likely from buffer overflow)
    if (*next).psize != (*s).csize {
        a_crash();
    }

    // Merge adjacent chunks if we need more space. This is not
    // a waste of time even if we fail to get enough space, because our
    // subsequent call to free would otherwise have to do the merge.
    let mut n1 = n0;
    if n > n1 && alloc_fwd(next) != 0 {
        n1 += chunk_size(next);
        next = next_chunk(next);
    }

    (*s).csize = n1 | 1;
    (*next).psize = n1 | 1;

    // If we got enough space, split off the excess and return
    if n <= n1 {
        trim(s, n);
        return chunk_to_mem(s);
    }

    // As a last resort, allocate a new chunk and copy to it.
    let new = malloc(n - OVERHEAD);
    if new.is_null() {
        // } == 0 as *mut c_void {
        return 0 as *mut c_void;
    }

    memcpy(new, p, n0 - OVERHEAD);
    free(chunk_to_mem(s));

    new
}

#[no_mangle]
pub unsafe extern "C" fn adjust_size(n: *mut usize) -> c_int {
    // Result of pointer difference must fit in ptrdiff_t.
    if *n - 1 > isize::MAX as usize - SIZE_ALIGN as usize - PAGE_SIZE as usize {
        if *n != 0 {
            set_errno(ENOMEM);
            return -1;
        } else {
            *n = SIZE_ALIGN;
            return 0;
        }
    }

    *n = (*n + OVERHEAD + SIZE_ALIGN - 1) & SIZE_MASK;
    return 0;
}

#[no_mangle]
pub unsafe extern "C" fn unbin(c: *mut chunk, i: c_int) {
    if (*c).prev == (*c).next {
        a_and_64(&mut mal.binmap, !(1u64 << i));
    }

    (*(*c).prev).next = (*c).next;
    (*(*c).next).prev = (*c).prev;
    (*c).csize |= 1;
    (*next_chunk(c)).psize |= 1;
}

#[no_mangle]
pub unsafe extern "C" fn alloc_fwd(c: *mut chunk) -> c_int {
    let mut i: c_int;
    let mut k: usize;

    while {
        k = (*c).csize;
        k & 1 == 0
    } {
        i = bin_index(k);
        lock_bin(i);
        if (*c).csize == k {
            unbin(c, i);
            unlock_bin(i);
            return 1;
        }
        unlock_bin(i);
    }

    0
}

#[no_mangle]
pub unsafe extern "C" fn alloc_rev(c: *mut chunk) -> c_int {
    let mut i: c_int;
    let mut k: usize;

    while {
        k = (*c).psize;
        k & 1 == 0
    } {
        i = bin_index(k);
        lock_bin(i);
        if (*c).psize == k {
            unbin(previous_chunk(c), i);
            unlock_bin(i);
            return 1;
        }
        unlock_bin(i);
    }
    0
}

// pretrim - trims a chunk _prior_ to removing it from its bin.
// Must be called with i as the ideal bin for size n, j the bin
// for the _free_ chunk self, and bin j locked.
#[no_mangle]
pub unsafe extern "C" fn pretrim(s: *mut chunk, n: usize, i: c_int, j: c_int) -> c_int {
    // We cannot pretrim if it would require re-binning.
    if j < 40 || (j < i + 3 && j != 63) {
        return 0;
    }

    let n1 = chunk_size(s);

    if n1 - n <= MMAP_THRESHOLD {
        return 0;
    }

    if bin_index(n1 - n) != j {
        return 0;
    }

    let next = next_chunk(s);
    let split = (s as *mut u8).offset(n as isize) as *mut chunk;

    (*split).prev = (*s).prev;
    (*split).next = (*s).next;
    (*(*split).prev).next = split;
    (*(*split).next).prev = split;
    (*split).psize = n | 1;
    (*split).csize = n1 - n;

    (*next).psize = n1 - n;

    (*s).csize = n | 1;

    return 1;
}

#[no_mangle]
pub unsafe extern "C" fn trim(s: *mut chunk, n: usize) {
    let n1 = chunk_size(s);

    if n >= n1 - DONTCARE {
        return;
    }

    let next = next_chunk(s);
    let split = (s as *mut u8).offset(n as isize) as *mut chunk;

    (*split).psize = n | 1;
    (*split).csize = (n1 - n) | 1;

    (*next).psize = (n1 - n) | 1;

    (*s).csize = n | 1;

    free(chunk_to_mem(split));
}

#[no_mangle]
pub unsafe extern "C" fn expand_heap(mut n: usize) -> *mut chunk {
    static mut heap_lock: Mutex<*mut c_void> = Mutex::new(0 as *mut c_void);

    let mut p: *mut c_void;
    let mut w: *mut chunk;

    // The argument n already accounts for the caller's chunk
    // overhead needs, but if the heap can't be extended in-place,
    // we need room for an extra zero-sized sentinel chunk.
    n += SIZE_ALIGN;

    let mut end = heap_lock.lock();

    p = __expand_heap(&mut n as *mut usize);

    if p as usize == 0 {
        return 0 as *mut chunk;
    }

    // If not just expanding existing space, we need to make a
    // new sentinel chunk below the allocated space.
    if p != *end {
        // Valid/safe because of the prologue increment.
        n -= SIZE_ALIGN;
        p = (p as *mut u8).offset(SIZE_ALIGN as isize) as *mut c_void;
        w = mem_to_chunk(p);
        (*w).psize = 0 | 1;
    }

    // Record new heap end and fill in footer.
    *end = (p as *mut u8).offset(n as isize) as *mut c_void;
    w = mem_to_chunk(*end);
    (*w).psize = n | 1;
    (*w).csize = 0 | 1;

    // Fill in header, which may be new or may be replacing a
    // zero-size sentinel header at the old end-of-heap.
    w = mem_to_chunk(p);
    (*w).csize = n | 1;

    w
}

unsafe fn mem_to_chunk(ptr: *mut c_void) -> *mut chunk {
    ((ptr as *mut u8).offset(-(OVERHEAD as isize))) as *mut chunk
}

unsafe fn chunk_to_mem(c: *mut chunk) -> *mut c_void {
    (c as *mut u8).offset(OVERHEAD as isize) as *mut c_void
}

unsafe fn bin_to_chunk(i: usize) -> *mut chunk {
    mem_to_chunk(((&mut mal.bins[i].head) as *mut *mut chunk as usize) as *mut c_void)
}

unsafe fn chunk_size(c: *mut chunk) -> usize { (*c).csize & ((-2i64) as usize) }

unsafe fn chunk_psize(c: *mut chunk) -> usize { (*c).psize & ((-2i64) as usize) }

unsafe fn previous_chunk(c: *mut chunk) -> *mut chunk {
    (c as *mut u8).offset(-(chunk_psize(c) as isize)) as *mut chunk
}

unsafe fn next_chunk(c: *mut chunk) -> *mut chunk {
    (c as *mut u8).offset(chunk_size(c) as isize) as *mut chunk
}

unsafe fn is_mmapped(c: *mut chunk) -> bool { ((*c).csize & 1) == 0 }

#[no_mangle]
pub unsafe extern "C" fn lock(lock: *mut c_int) {
    while a_swap(lock, 1) != 0 {
        __wait(lock, lock.offset(1), 1, 1);
    }
}

#[no_mangle]
pub unsafe extern "C" fn unlock(lock: *mut c_int) {
    if *lock != 0 {
        a_store(lock, 0);
        if *lock.offset(1) != 0 {
            __wake(lock as *mut c_void, 1, 1);
        }
    }
}

#[no_mangle]
pub unsafe extern "C" fn unlock_bin(i: c_int) { unlock(&mut mal.bins[i as usize].lock[0]); }

#[no_mangle]
pub unsafe extern "C" fn lock_bin(i: c_int) {
    let i = i as usize;
    lock(&mut mal.bins[i].lock[0]);

    if mal.bins[i].head as usize == 0 {
        mal.bins[i].tail = bin_to_chunk(i);
        mal.bins[i].head = mal.bins[i].tail;
    }
}
