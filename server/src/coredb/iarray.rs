/*
 * Created on Sun Jul 04 2021
 *
 * This file is a part of Skytable
 * Skytable (formerly known as TerrabaseDB or Skybase) is a free and open-source
 * NoSQL database written by Sayan Nandan ("the Author") with the
 * vision to provide flexibility in data modelling without compromising
 * on performance, queryability or scalability.
 *
 * Copyright (c) 2021, Sayan Nandan <ohsayan@outlook.com>
 *
 * This program is free software: you can redistribute it and/or modify
 * it under the terms of the GNU Affero General Public License as published by
 * the Free Software Foundation, either version 3 of the License, or
 * (at your option) any later version.
 *
 * This program is distributed in the hope that it will be useful,
 * but WITHOUT ANY WARRANTY; without even the implied warranty of
 * MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
 * GNU Affero General Public License for more details.
 *
 * You should have received a copy of the GNU Affero General Public License
 * along with this program. If not, see <https://www.gnu.org/licenses/>.
 *
*/

#![allow(dead_code)] // TODO(@ohsayan): Remove this once we're done

use core::alloc::Layout;
use core::borrow::Borrow;
use core::borrow::BorrowMut;
use core::cmp;
use core::hash::{self, Hash};
use core::slice;
use std::alloc as std_alloc;
use std::fmt;
use std::mem;
use std::mem::ManuallyDrop;
use std::mem::MaybeUninit;
use std::ops;
use std::ptr;
use std::ptr::NonNull;

pub trait MemoryBlock {
    type LayoutItem;
    fn size() -> usize;
}

macro_rules! impl_memoryblock_stack_array_with_size {
    ($($n:expr),*) => {
        $(
            impl<T> MemoryBlock for [T; $n] {
                type LayoutItem = T;
                fn size() -> usize {
                    $n
                }
            }
        )*
    };
}

// only power of two stacks
impl_memoryblock_stack_array_with_size!(2, 4, 8, 16, 32, 64, 128, 256);

pub union InlineArray<A: MemoryBlock> {
    stack: ManuallyDrop<MaybeUninit<A>>,
    heap_ptr_len: (*mut A::LayoutItem, usize),
}

impl<A: MemoryBlock> InlineArray<A> {
    unsafe fn stack_ptr(&self) -> *const A::LayoutItem {
        self.stack.as_ptr() as *const _
    }
    unsafe fn stack_ptr_mut(&mut self) -> *mut A::LayoutItem {
        self.stack.as_mut_ptr() as *mut _
    }
    fn from_stack(stack: MaybeUninit<A>) -> Self {
        Self {
            stack: ManuallyDrop::new(stack),
        }
    }
    fn from_heap_ptr(start_ptr: *mut A::LayoutItem, len: usize) -> Self {
        Self {
            heap_ptr_len: (start_ptr, len),
        }
    }
    unsafe fn heap_size(&self) -> usize {
        self.heap_ptr_len.1
    }
    unsafe fn heap_ptr(&self) -> *const A::LayoutItem {
        self.heap_ptr_len.0
    }
    unsafe fn heap_ptr_mut(&mut self) -> *mut A::LayoutItem {
        self.heap_ptr_len.0 as *mut _
    }
    unsafe fn heap_size_mut(&mut self) -> &mut usize {
        &mut self.heap_ptr_len.1
    }
    unsafe fn heap(&self) -> (*mut A::LayoutItem, usize) {
        self.heap_ptr_len
    }
    unsafe fn heap_mut(&mut self) -> (*mut A::LayoutItem, &mut usize) {
        (self.heap_ptr_mut(), &mut self.heap_ptr_len.1)
    }
}

pub fn calculate_memory_layout<T>(count: usize) -> Result<Layout, ()> {
    let size = mem::size_of::<T>().checked_mul(count).ok_or(())?;
    // err is cap overflow
    let alignment = mem::align_of::<T>();
    Layout::from_size_align(size, alignment).map_err(|_| ())
}

/// Use the global allocator to deallocate the memory block for the given starting ptr
/// upto the given capacity
unsafe fn dealloc<T>(start_ptr: *mut T, capacity: usize) {
    std_alloc::dealloc(
        start_ptr as *mut u8,
        calculate_memory_layout::<T>(capacity).expect("Memory capacity overflow"),
    )
}

// Break free from Rust's aliasing rules with these typedefs
type DataptrLenptrCapacity<T> = (*const T, usize, usize);
type DataptrLenptrCapacityMut<'a, T> = (*mut T, &'a mut usize, usize);

pub struct IArray<A: MemoryBlock> {
    cap: usize,
    store: InlineArray<A>,
}

impl<A: MemoryBlock> IArray<A> {
    pub fn new() -> IArray<A> {
        Self {
            cap: 0,
            store: InlineArray::from_stack(MaybeUninit::uninit()),
        }
    }
    pub fn from_vec(mut vec: Vec<A::LayoutItem>) -> Self {
        if vec.capacity() <= Self::stack_capacity() {
            let mut store = InlineArray::<A>::from_stack(MaybeUninit::uninit());
            let len = vec.len();
            unsafe {
                ptr::copy_nonoverlapping(vec.as_ptr(), store.stack_ptr_mut(), len);
            }
            // done with the copy
            Self { cap: len, store }
        } else {
            let (start_ptr, cap, len) = (vec.as_mut_ptr(), vec.capacity(), vec.len());
            // leak the vec
            mem::forget(vec);
            IArray {
                cap,
                store: InlineArray::from_heap_ptr(start_ptr, len),
            }
        }
    }
    fn stack_capacity() -> usize {
        if mem::size_of::<A::LayoutItem>() > 0 {
            // not a ZST, so cap of array
            A::size()
        } else {
            // ZST. Just pile up some garbage and say that we have infinity
            usize::MAX
        }
    }
    fn meta_triple(&self) -> DataptrLenptrCapacity<A::LayoutItem> {
        unsafe {
            if self.went_off_stack() {
                let (data_ptr, len_ptr) = self.store.heap();
                (data_ptr, len_ptr, self.cap)
            } else {
                // still on stack
                (self.store.stack_ptr(), self.cap, Self::stack_capacity())
            }
        }
    }
    fn meta_triple_mut(&mut self) -> DataptrLenptrCapacityMut<A::LayoutItem> {
        unsafe {
            if self.went_off_stack() {
                // get heap
                let (data_ptr, len_ptr) = self.store.heap_mut();
                (data_ptr, len_ptr, self.cap)
            } else {
                // still on stack
                (
                    self.store.stack_ptr_mut(),
                    &mut self.cap,
                    Self::stack_capacity(),
                )
            }
        }
    }
    fn get_data_ptr_mut(&mut self) -> *mut A::LayoutItem {
        if self.went_off_stack() {
            // get the heap ptr
            unsafe { self.store.heap_ptr_mut() }
        } else {
            // get the stack ptr
            unsafe { self.store.stack_ptr_mut() }
        }
    }
    fn went_off_stack(&self) -> bool {
        self.cap > Self::stack_capacity()
    }
    pub fn len(&self) -> usize {
        if self.went_off_stack() {
            // so we're off the stack
            unsafe { self.store.heap_size() }
        } else {
            // still on the stack
            self.cap
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn get_capacity(&self) -> usize {
        if self.went_off_stack() {
            self.cap
        } else {
            Self::stack_capacity()
        }
    }
    fn grow_block(&mut self, new_cap: usize) {
        // infallible
        unsafe {
            let (data_ptr, &mut len, cap) = self.meta_triple_mut();
            let still_on_stack = !self.went_off_stack();
            assert!(new_cap > len);
            if new_cap <= Self::stack_capacity() {
                if still_on_stack {
                    return;
                }
                // no branch
                self.store = InlineArray::from_stack(MaybeUninit::uninit());
                ptr::copy_nonoverlapping(data_ptr, self.store.stack_ptr_mut(), len);
                self.cap = len;
                dealloc(data_ptr, cap);
            } else if new_cap != cap {
                let layout =
                    calculate_memory_layout::<A::LayoutItem>(new_cap).expect("Capacity overflow");
                assert!(layout.size() > 0);
                let new_alloc;
                if still_on_stack {
                    new_alloc = NonNull::new(std_alloc::alloc(layout).cast())
                        .expect("Allocation error")
                        .as_ptr();
                    ptr::copy_nonoverlapping(data_ptr, new_alloc, len);
                } else {
                    // not on stack
                    let old_layout =
                        calculate_memory_layout::<A::LayoutItem>(cap).expect("Capacity overflow");
                    // realloc the earlier buffer
                    let new_memory_block_ptr =
                        std_alloc::realloc(data_ptr as *mut _, old_layout, layout.size());
                    new_alloc = NonNull::new(new_memory_block_ptr.cast())
                        .expect("Allocation error")
                        .as_ptr();
                }
                self.store = InlineArray::from_heap_ptr(new_alloc, len);
                self.cap = new_cap;
            }
        }
    }
    fn reserve(&mut self, additional: usize) {
        let (_, &mut len, cap) = self.meta_triple_mut();
        if cap - len >= additional {
            // already have enough space
            return;
        }
        let new_cap = len
            .checked_add(additional)
            .map(usize::next_power_of_two)
            .expect("Capacity overflow");
        self.grow_block(new_cap)
    }
    pub fn push(&mut self, val: A::LayoutItem) {
        unsafe {
            let (mut data_ptr, mut len, cap) = self.meta_triple_mut();
            if (*len).eq(&cap) {
                self.reserve(1);
                let (heap_ptr, heap_len) = self.store.heap_mut();
                data_ptr = heap_ptr;
                len = heap_len;
            }
            ptr::write(data_ptr.add(*len), val);
            *len += 1;
        }
    }
    pub fn pop(&mut self) -> Option<A::LayoutItem> {
        unsafe {
            let (data_ptr, len_mut, _cap) = self.meta_triple_mut();
            if *len_mut == 0 {
                // empty man, what do you want?
                None
            } else {
                // there's something
                let last_index = *len_mut - 1;
                // we'll say that it's gone
                *len_mut = last_index;
                // but only read it now from the offset
                Some(ptr::read(data_ptr.add(last_index)))
            }
        }
    }
    pub fn shrink(&mut self) {
        if self.went_off_stack() {
            // it's off the stack, so no chance of moving back to the stack
            return;
        }
        let current_len = self.len();
        if Self::stack_capacity() >= current_len {
            // we have a chance of copying this over to our stack
            unsafe {
                let (data_ptr, len) = self.store.heap();
                self.store = InlineArray::from_stack(MaybeUninit::uninit());
                // copy to stack
                ptr::copy_nonoverlapping(data_ptr, self.store.stack_ptr_mut(), len);
                // now deallocate the heap
                dealloc(data_ptr, self.cap);
                self.cap = len;
            }
        } else if self.get_capacity() > current_len {
            // more capacity than current len? so we're on the heap
            // grow the block to place it on stack (this will dealloc the heap)
            self.grow_block(current_len);
        }
    }
    pub fn truncate(&mut self, target_len: usize) {
        unsafe {
            let (data_ptr, len_mut, _cap) = self.meta_triple_mut();
            while target_len < *len_mut {
                // get the last index
                let last_index = *len_mut - 1;
                // drop it
                ptr::drop_in_place(data_ptr.add(last_index));
                // update the length
                *len_mut = last_index;
            }
        }
    }
    pub fn clear(&mut self) {
        // chop off the whole place
        self.truncate(0);
    }
    unsafe fn set_len(&mut self, new_len: usize) {
        let (_dataptr, len_mut, _cap) = self.meta_triple_mut();
        *len_mut = new_len;
    }
}

impl<A: MemoryBlock> IArray<A>
where
    A::LayoutItem: Copy,
{
    pub fn from_slice(slice: &[A::LayoutItem]) -> Self {
        // FIXME(@ohsayan): Could we have had this as a From::from() method?
        let slice_len = slice.len();
        if slice_len <= Self::stack_capacity() {
            // so we can place this thing on the stack
            let mut new_stack = MaybeUninit::uninit();
            unsafe {
                ptr::copy_nonoverlapping(
                    slice.as_ptr(),
                    new_stack.as_mut_ptr() as *mut A::LayoutItem,
                    slice_len,
                );
            }
            Self {
                cap: slice_len,
                store: InlineArray::from_stack(new_stack),
            }
        } else {
            // argggh, on the heap
            let mut v = slice.to_vec();
            let (ptr, cap) = (v.as_mut_ptr(), v.capacity());
            // leak it
            mem::forget(v);
            Self {
                cap,
                store: InlineArray::from_heap_ptr(ptr, slice_len),
            }
        }
    }
    pub fn insert_slice_at_index(&mut self, slice: &[A::LayoutItem], index: usize) {
        self.reserve(slice.len());
        let len = self.len();
        // only catch during tests
        debug_assert!(index <= len);
        unsafe {
            let slice_ptr = slice.as_ptr();
            // we need to add it from the end of the current item
            let data_ptr_start = self.get_data_ptr_mut().add(len);
            // copy the slice over
            ptr::copy(data_ptr_start, data_ptr_start.add(slice.len()), len - index);
            ptr::copy_nonoverlapping(slice_ptr, data_ptr_start, slice.len());
            self.set_len(len + slice.len());
        }
    }
    pub fn extend_from_slice(&mut self, slice: &[A::LayoutItem]) {
        // at our len because we're appending it to the end
        self.insert_slice_at_index(slice, self.len())
    }
}

impl<A: MemoryBlock> ops::Deref for IArray<A> {
    type Target = [A::LayoutItem];
    fn deref(&self) -> &Self::Target {
        unsafe {
            let (start_ptr, len, _) = self.meta_triple();
            slice::from_raw_parts(start_ptr, len)
        }
    }
}

impl<A: MemoryBlock> ops::DerefMut for IArray<A> {
    fn deref_mut(&mut self) -> &mut [A::LayoutItem] {
        unsafe {
            let (start_ptr, &mut len, _) = self.meta_triple_mut();
            slice::from_raw_parts_mut(start_ptr, len)
        }
    }
}

impl<A: MemoryBlock> AsRef<[A::LayoutItem]> for IArray<A> {
    fn as_ref(&self) -> &[A::LayoutItem] {
        self
    }
}

impl<A: MemoryBlock> AsMut<[A::LayoutItem]> for IArray<A> {
    fn as_mut(&mut self) -> &mut [A::LayoutItem] {
        self
    }
}

// we need these for our coremap

impl<A: MemoryBlock> Borrow<[A::LayoutItem]> for IArray<A> {
    fn borrow(&self) -> &[A::LayoutItem] {
        self
    }
}

impl<A: MemoryBlock> BorrowMut<[A::LayoutItem]> for IArray<A> {
    fn borrow_mut(&mut self) -> &mut [A::LayoutItem] {
        self
    }
}

impl<A: MemoryBlock> Drop for IArray<A> {
    fn drop(&mut self) {
        unsafe {
            if self.went_off_stack() {
                // free the heap
                let (ptr, len) = self.store.heap();
                // let vec's destructor do the work
                mem::drop(Vec::from_raw_parts(ptr, len, self.cap));
            } else {
                // on stack? get self as a slice and destruct it
                ptr::drop_in_place(&mut self[..]);
            }
        }
    }
}

pub struct LenScopeGuard<'a> {
    real_ref: &'a mut usize,
    temp: usize,
}

impl<'a> LenScopeGuard<'a> {
    pub fn new(real_ref: &'a mut usize) -> Self {
        let ret = *real_ref;
        Self {
            real_ref,
            temp: ret,
        }
    }
    pub fn incr(&mut self) {
        self.temp += 1;
    }
    pub fn get_temp(&self) -> usize {
        self.temp
    }
}

impl<'a> Drop for LenScopeGuard<'a> {
    fn drop(&mut self) {
        *self.real_ref = self.temp;
    }
}

impl<A: MemoryBlock> Extend<A::LayoutItem> for IArray<A> {
    fn extend<I: IntoIterator<Item = A::LayoutItem>>(&mut self, iterable: I) {
        let mut iter = iterable.into_iter();
        let (lower_bound, _upper_bound) = iter.size_hint();
        // reserve the lower bound; we really want it on the stack
        self.reserve(lower_bound);

        unsafe {
            let (data_ptr, len_ptr, cap) = self.meta_triple_mut();
            let mut len = LenScopeGuard::new(len_ptr);
            while len.get_temp() < cap {
                if let Some(out) = iter.next() {
                    ptr::write(data_ptr.add(len.get_temp()), out);
                    len.incr();
                } else {
                    return;
                }
            }
        }
        // still have something left, probably a heap alloc :(
        for elem in iter {
            self.push(elem);
        }
    }
}

impl<A: MemoryBlock> fmt::Debug for IArray<A>
where
    A::LayoutItem: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        f.debug_list().entries(self.iter()).finish()
    }
}

impl<A: MemoryBlock, B: MemoryBlock> PartialEq<IArray<B>> for IArray<A>
where
    A::LayoutItem: PartialEq<B::LayoutItem>,
{
    fn eq(&self, rhs: &IArray<B>) -> bool {
        self[..] == rhs[..]
    }
}

impl<A: MemoryBlock> Eq for IArray<A> where A::LayoutItem: Eq {}

impl<A: MemoryBlock> PartialOrd for IArray<A>
where
    A::LayoutItem: PartialOrd,
{
    fn partial_cmp(&self, rhs: &IArray<A>) -> Option<cmp::Ordering> {
        PartialOrd::partial_cmp(&**self, &**rhs)
    }
}

impl<A: MemoryBlock> Ord for IArray<A>
where
    A::LayoutItem: Ord,
{
    fn cmp(&self, rhs: &IArray<A>) -> cmp::Ordering {
        Ord::cmp(&**self, &**rhs)
    }
}

impl<A: MemoryBlock> Hash for IArray<A>
where
    A::LayoutItem: Hash,
{
    fn hash<H>(&self, hasher: &mut H)
    where
        H: hash::Hasher,
    {
        (**self).hash(hasher)
    }
}

#[test]
fn test_equality() {
    let mut x: IArray<[u8; 32]> = IArray::new();
    x.extend_from_slice("AVeryGoodKeyspaceName".as_bytes());
    assert_eq!(x, {
        let mut i = IArray::<[u8; 64]>::new();
        "AVeryGoodKeyspaceName"
            .chars()
            .for_each(|char| i.push(char as u8));
        i
    })
}
