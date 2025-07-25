use super::{
    PositionIterInternal, PyBytes, PyBytesRef, PyGenericAlias, PyInt, PyListRef, PySlice, PyStr,
    PyStrRef, PyTuple, PyTupleRef, PyType, PyTypeRef,
};
use crate::{
    AsObject, Context, Py, PyObject, PyObjectRef, PyPayload, PyRef, PyResult,
    TryFromBorrowedObject, TryFromObject, VirtualMachine, atomic_func,
    buffer::FormatSpec,
    bytes_inner::bytes_to_hex,
    class::PyClassImpl,
    common::{
        borrow::{BorrowedValue, BorrowedValueMut},
        hash::PyHash,
        lock::OnceCell,
    },
    convert::ToPyObject,
    function::Either,
    function::{FuncArgs, OptionalArg, PyComparisonValue},
    protocol::{
        BufferDescriptor, BufferMethods, PyBuffer, PyIterReturn, PyMappingMethods,
        PySequenceMethods, VecBuffer,
    },
    sliceable::SequenceIndexOp,
    types::{
        AsBuffer, AsMapping, AsSequence, Comparable, Constructor, Hashable, IterNext, Iterable,
        PyComparisonOp, Representable, SelfIter, Unconstructible,
    },
};
use crossbeam_utils::atomic::AtomicCell;
use itertools::Itertools;
use rustpython_common::lock::PyMutex;
use std::sync::LazyLock;
use std::{cmp::Ordering, fmt::Debug, mem::ManuallyDrop, ops::Range};

#[derive(FromArgs)]
pub struct PyMemoryViewNewArgs {
    object: PyObjectRef,
}

#[pyclass(module = false, name = "memoryview")]
#[derive(Debug)]
pub struct PyMemoryView {
    // avoid double release when memoryview had released the buffer before drop
    buffer: ManuallyDrop<PyBuffer>,
    // the released memoryview does not mean the buffer is destroyed
    // because the possible another memoryview is viewing from it
    released: AtomicCell<bool>,
    // start does NOT mean the bytes before start will not be visited,
    // it means the point we starting to get the absolute position via
    // the needle
    start: usize,
    format_spec: FormatSpec,
    // memoryview's options could be different from buffer's options
    desc: BufferDescriptor,
    hash: OnceCell<PyHash>,
    // exports
    // memoryview has no exports count by itself
    // instead it relay on the buffer it viewing to maintain the count
}

impl Constructor for PyMemoryView {
    type Args = PyMemoryViewNewArgs;

    fn py_new(cls: PyTypeRef, args: Self::Args, vm: &VirtualMachine) -> PyResult {
        let zelf = Self::from_object(&args.object, vm)?;
        zelf.into_ref_with_type(vm, cls).map(Into::into)
    }
}

impl PyMemoryView {
    fn parse_format(format: &str, vm: &VirtualMachine) -> PyResult<FormatSpec> {
        FormatSpec::parse(format.as_bytes(), vm)
    }

    /// this should be the main entrance to create the memoryview
    /// to avoid the chained memoryview
    pub fn from_object(obj: &PyObject, vm: &VirtualMachine) -> PyResult<Self> {
        if let Some(other) = obj.payload::<Self>() {
            Ok(other.new_view())
        } else {
            let buffer = PyBuffer::try_from_borrowed_object(vm, obj)?;
            Self::from_buffer(buffer, vm)
        }
    }

    /// don't use this function to create the memoryview if the buffer is exporting
    /// via another memoryview, use PyMemoryView::new_view() or PyMemoryView::from_object
    /// to reduce the chain
    pub fn from_buffer(buffer: PyBuffer, vm: &VirtualMachine) -> PyResult<Self> {
        // when we get a buffer means the buffered object is size locked
        // so we can assume the buffer's options will never change as long
        // as memoryview is still alive
        let format_spec = Self::parse_format(&buffer.desc.format, vm)?;
        let desc = buffer.desc.clone();

        Ok(Self {
            buffer: ManuallyDrop::new(buffer),
            released: AtomicCell::new(false),
            start: 0,
            format_spec,
            desc,
            hash: OnceCell::new(),
        })
    }

    /// don't use this function to create the memoryview if the buffer is exporting
    /// via another memoryview, use PyMemoryView::new_view() or PyMemoryView::from_object
    /// to reduce the chain
    pub fn from_buffer_range(
        buffer: PyBuffer,
        range: Range<usize>,
        vm: &VirtualMachine,
    ) -> PyResult<Self> {
        let mut zelf = Self::from_buffer(buffer, vm)?;

        zelf.init_range(range, 0);
        zelf.init_len();
        Ok(zelf)
    }

    /// this should be the only way to create a memoryview from another memoryview
    pub fn new_view(&self) -> Self {
        let zelf = Self {
            buffer: self.buffer.clone(),
            released: AtomicCell::new(false),
            start: self.start,
            format_spec: self.format_spec.clone(),
            desc: self.desc.clone(),
            hash: OnceCell::new(),
        };
        zelf.buffer.retain();
        zelf
    }

    fn try_not_released(&self, vm: &VirtualMachine) -> PyResult<()> {
        if self.released.load() {
            Err(vm.new_value_error("operation forbidden on released memoryview object"))
        } else {
            Ok(())
        }
    }

    fn getitem_by_idx(&self, i: isize, vm: &VirtualMachine) -> PyResult {
        if self.desc.ndim() != 1 {
            return Err(
                vm.new_not_implemented_error("multi-dimensional sub-views are not implemented")
            );
        }
        let (shape, stride, suboffset) = self.desc.dim_desc[0];
        let index = i
            .wrapped_at(shape)
            .ok_or_else(|| vm.new_index_error("index out of range"))?;
        let index = index as isize * stride + suboffset;
        let pos = (index + self.start as isize) as usize;
        self.unpack_single(pos, vm)
    }

    fn getitem_by_slice(&self, slice: &PySlice, vm: &VirtualMachine) -> PyResult {
        let mut other = self.new_view();
        other.init_slice(slice, 0, vm)?;
        other.init_len();

        Ok(other.into_ref(&vm.ctx).into())
    }

    fn getitem_by_multi_idx(&self, indexes: &[isize], vm: &VirtualMachine) -> PyResult {
        let pos = self.pos_from_multi_index(indexes, vm)?;
        let bytes = self.buffer.obj_bytes();
        format_unpack(&self.format_spec, &bytes[pos..pos + self.desc.itemsize], vm)
    }

    fn setitem_by_idx(&self, i: isize, value: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        if self.desc.ndim() != 1 {
            return Err(vm.new_not_implemented_error("sub-views are not implemented"));
        }
        let (shape, stride, suboffset) = self.desc.dim_desc[0];
        let index = i
            .wrapped_at(shape)
            .ok_or_else(|| vm.new_index_error("index out of range"))?;
        let index = index as isize * stride + suboffset;
        let pos = (index + self.start as isize) as usize;
        self.pack_single(pos, value, vm)
    }

    fn setitem_by_multi_idx(
        &self,
        indexes: &[isize],
        value: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        let pos = self.pos_from_multi_index(indexes, vm)?;
        self.pack_single(pos, value, vm)
    }

    fn pack_single(&self, pos: usize, value: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        let mut bytes = self.buffer.obj_bytes_mut();
        // TODO: Optimize
        let data = self.format_spec.pack(vec![value], vm).map_err(|_| {
            vm.new_type_error(format!(
                "memoryview: invalid type for format '{}'",
                &self.desc.format
            ))
        })?;
        bytes[pos..pos + self.desc.itemsize].copy_from_slice(&data);
        Ok(())
    }

    fn unpack_single(&self, pos: usize, vm: &VirtualMachine) -> PyResult {
        let bytes = self.buffer.obj_bytes();
        // TODO: Optimize
        self.format_spec
            .unpack(&bytes[pos..pos + self.desc.itemsize], vm)
            .map(|x| {
                if x.len() == 1 {
                    x[0].to_owned()
                } else {
                    x.into()
                }
            })
    }

    fn pos_from_multi_index(&self, indexes: &[isize], vm: &VirtualMachine) -> PyResult<usize> {
        match indexes.len().cmp(&self.desc.ndim()) {
            Ordering::Less => {
                return Err(vm.new_not_implemented_error("sub-views are not implemented"));
            }
            Ordering::Greater => {
                return Err(vm.new_type_error(format!(
                    "cannot index {}-dimension view with {}-element tuple",
                    self.desc.ndim(),
                    indexes.len()
                )));
            }
            Ordering::Equal => (),
        }

        let pos = self.desc.position(indexes, vm)?;
        let pos = (pos + self.start as isize) as usize;
        Ok(pos)
    }

    fn init_len(&mut self) {
        let product: usize = self.desc.dim_desc.iter().map(|x| x.0).product();
        self.desc.len = product * self.desc.itemsize;
    }

    fn init_range(&mut self, range: Range<usize>, dim: usize) {
        let (shape, stride, _) = self.desc.dim_desc[dim];
        debug_assert!(shape >= range.len());

        let mut is_adjusted = false;
        for (_, _, suboffset) in self.desc.dim_desc.iter_mut().rev() {
            if *suboffset != 0 {
                *suboffset += stride * range.start as isize;
                is_adjusted = true;
                break;
            }
        }
        if !is_adjusted {
            // no suboffset set, stride must be positive
            self.start += stride as usize * range.start;
        }
        let new_len = range.len();
        self.desc.dim_desc[dim].0 = new_len;
    }

    fn init_slice(&mut self, slice: &PySlice, dim: usize, vm: &VirtualMachine) -> PyResult<()> {
        let (shape, stride, _) = self.desc.dim_desc[dim];
        let slice = slice.to_saturated(vm)?;
        let (range, step, slice_len) = slice.adjust_indices(shape);

        let mut is_adjusted_suboffset = false;
        for (_, _, suboffset) in self.desc.dim_desc.iter_mut().rev() {
            if *suboffset != 0 {
                *suboffset += stride * range.start as isize;
                is_adjusted_suboffset = true;
                break;
            }
        }
        if !is_adjusted_suboffset {
            // no suboffset set, stride must be positive
            self.start += stride as usize
                * if step.is_negative() {
                    range.end - 1
                } else {
                    range.start
                };
        }
        self.desc.dim_desc[dim].0 = slice_len;
        self.desc.dim_desc[dim].1 *= step;

        Ok(())
    }

    fn _to_list(
        &self,
        bytes: &[u8],
        mut index: isize,
        dim: usize,
        vm: &VirtualMachine,
    ) -> PyResult<PyListRef> {
        let (shape, stride, suboffset) = self.desc.dim_desc[dim];
        if dim + 1 == self.desc.ndim() {
            let mut v = Vec::with_capacity(shape);
            for _ in 0..shape {
                let pos = index + suboffset;
                let pos = (pos + self.start as isize) as usize;
                let obj =
                    format_unpack(&self.format_spec, &bytes[pos..pos + self.desc.itemsize], vm)?;
                v.push(obj);
                index += stride;
            }
            return Ok(vm.ctx.new_list(v));
        }

        let mut v = Vec::with_capacity(shape);
        for _ in 0..shape {
            let obj = self._to_list(bytes, index + suboffset, dim + 1, vm)?.into();
            v.push(obj);
            index += stride;
        }
        Ok(vm.ctx.new_list(v))
    }

    fn eq(zelf: &Py<Self>, other: &PyObject, vm: &VirtualMachine) -> PyResult<bool> {
        if zelf.is(other) {
            return Ok(true);
        }
        if zelf.released.load() {
            return Ok(false);
        }

        if let Some(other) = other.payload::<Self>() {
            if other.released.load() {
                return Ok(false);
            }
        }

        let other = match PyBuffer::try_from_borrowed_object(vm, other) {
            Ok(buf) => buf,
            Err(_) => return Ok(false),
        };

        if !is_equiv_shape(&zelf.desc, &other.desc) {
            return Ok(false);
        }

        let a_itemsize = zelf.desc.itemsize;
        let b_itemsize = other.desc.itemsize;
        let a_format_spec = &zelf.format_spec;
        let b_format_spec = &Self::parse_format(&other.desc.format, vm)?;

        if zelf.desc.ndim() == 0 {
            let a_val = format_unpack(a_format_spec, &zelf.buffer.obj_bytes()[..a_itemsize], vm)?;
            let b_val = format_unpack(b_format_spec, &other.obj_bytes()[..b_itemsize], vm)?;
            return vm.bool_eq(&a_val, &b_val);
        }

        // TODO: optimize cmp by format
        let mut ret = Ok(true);
        let a_bytes = zelf.buffer.obj_bytes();
        let b_bytes = other.obj_bytes();
        zelf.desc.zip_eq(&other.desc, false, |a_range, b_range| {
            let a_range = (a_range.start + zelf.start as isize) as usize
                ..(a_range.end + zelf.start as isize) as usize;
            let b_range = b_range.start as usize..b_range.end as usize;
            let a_val = match format_unpack(a_format_spec, &a_bytes[a_range], vm) {
                Ok(val) => val,
                Err(e) => {
                    ret = Err(e);
                    return true;
                }
            };
            let b_val = match format_unpack(b_format_spec, &b_bytes[b_range], vm) {
                Ok(val) => val,
                Err(e) => {
                    ret = Err(e);
                    return true;
                }
            };
            ret = vm.bool_eq(&a_val, &b_val);
            if let Ok(b) = ret { !b } else { true }
        });
        ret
    }

    fn obj_bytes(&self) -> BorrowedValue<'_, [u8]> {
        if self.desc.is_contiguous() {
            BorrowedValue::map(self.buffer.obj_bytes(), |x| {
                &x[self.start..self.start + self.desc.len]
            })
        } else {
            BorrowedValue::map(self.buffer.obj_bytes(), |x| &x[self.start..])
        }
    }

    fn obj_bytes_mut(&self) -> BorrowedValueMut<'_, [u8]> {
        if self.desc.is_contiguous() {
            BorrowedValueMut::map(self.buffer.obj_bytes_mut(), |x| {
                &mut x[self.start..self.start + self.desc.len]
            })
        } else {
            BorrowedValueMut::map(self.buffer.obj_bytes_mut(), |x| &mut x[self.start..])
        }
    }

    fn as_contiguous(&self) -> Option<BorrowedValue<'_, [u8]>> {
        self.desc.is_contiguous().then(|| {
            BorrowedValue::map(self.buffer.obj_bytes(), |x| {
                &x[self.start..self.start + self.desc.len]
            })
        })
    }

    fn _as_contiguous_mut(&self) -> Option<BorrowedValueMut<'_, [u8]>> {
        self.desc.is_contiguous().then(|| {
            BorrowedValueMut::map(self.buffer.obj_bytes_mut(), |x| {
                &mut x[self.start..self.start + self.desc.len]
            })
        })
    }

    fn append_to(&self, buf: &mut Vec<u8>) {
        if let Some(bytes) = self.as_contiguous() {
            buf.extend_from_slice(&bytes);
        } else {
            buf.reserve(self.desc.len);
            let bytes = &*self.buffer.obj_bytes();
            self.desc.for_each_segment(true, |range| {
                let start = (range.start + self.start as isize) as usize;
                let end = (range.end + self.start as isize) as usize;
                buf.extend_from_slice(&bytes[start..end]);
            })
        }
    }

    fn contiguous_or_collect<R, F: FnOnce(&[u8]) -> R>(&self, f: F) -> R {
        let borrowed;
        let mut collected;
        let v = if let Some(bytes) = self.as_contiguous() {
            borrowed = bytes;
            &*borrowed
        } else {
            collected = vec![];
            self.append_to(&mut collected);
            &collected
        };
        f(v)
    }

    /// clone data from memoryview
    /// keep the shape, convert to contiguous
    pub fn to_contiguous(&self, vm: &VirtualMachine) -> PyBuffer {
        let mut data = vec![];
        self.append_to(&mut data);

        if self.desc.ndim() == 0 {
            return VecBuffer::from(data)
                .into_ref(&vm.ctx)
                .into_pybuffer_with_descriptor(self.desc.clone());
        }

        let mut dim_desc = self.desc.dim_desc.clone();
        dim_desc.last_mut().unwrap().1 = self.desc.itemsize as isize;
        dim_desc.last_mut().unwrap().2 = 0;
        for i in (0..dim_desc.len() - 1).rev() {
            dim_desc[i].1 = dim_desc[i + 1].1 * dim_desc[i + 1].0 as isize;
            dim_desc[i].2 = 0;
        }

        let desc = BufferDescriptor {
            len: self.desc.len,
            readonly: self.desc.readonly,
            itemsize: self.desc.itemsize,
            format: self.desc.format.clone(),
            dim_desc,
        };

        VecBuffer::from(data)
            .into_ref(&vm.ctx)
            .into_pybuffer_with_descriptor(desc)
    }
}

impl Py<PyMemoryView> {
    fn setitem_by_slice(
        &self,
        slice: &PySlice,
        src: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        if self.desc.ndim() != 1 {
            return Err(vm.new_not_implemented_error("sub-view are not implemented"));
        }

        let mut dest = self.new_view();
        dest.init_slice(slice, 0, vm)?;
        dest.init_len();

        if self.is(&src) {
            return if !is_equiv_structure(&self.desc, &dest.desc) {
                Err(vm.new_value_error(
                    "memoryview assignment: lvalue and rvalue have different structures",
                ))
            } else {
                // assign self[:] to self
                Ok(())
            };
        };

        let src = if let Some(src) = src.downcast_ref::<PyMemoryView>() {
            if self.buffer.obj.is(&src.buffer.obj) {
                src.to_contiguous(vm)
            } else {
                AsBuffer::as_buffer(src, vm)?
            }
        } else {
            PyBuffer::try_from_object(vm, src)?
        };

        if !is_equiv_structure(&src.desc, &dest.desc) {
            return Err(vm.new_value_error(
                "memoryview assignment: lvalue and rvalue have different structures",
            ));
        }

        let mut bytes_mut = dest.buffer.obj_bytes_mut();
        let src_bytes = src.obj_bytes();
        dest.desc.zip_eq(&src.desc, true, |a_range, b_range| {
            let a_range = (a_range.start + dest.start as isize) as usize
                ..(a_range.end + dest.start as isize) as usize;
            let b_range = b_range.start as usize..b_range.end as usize;
            bytes_mut[a_range].copy_from_slice(&src_bytes[b_range]);
            false
        });

        Ok(())
    }
}

#[pyclass(with(
    Py,
    Hashable,
    Comparable,
    AsBuffer,
    AsMapping,
    AsSequence,
    Constructor,
    Iterable,
    Representable
))]
impl PyMemoryView {
    // TODO: Uncomment when Python adds __class_getitem__ to memoryview
    // #[pyclassmethod]
    fn __class_getitem__(cls: PyTypeRef, args: PyObjectRef, vm: &VirtualMachine) -> PyGenericAlias {
        PyGenericAlias::from_args(cls, args, vm)
    }

    #[pymethod]
    pub fn release(&self) {
        if self.released.compare_exchange(false, true).is_ok() {
            self.buffer.release();
        }
    }

    #[pygetset]
    fn obj(&self, vm: &VirtualMachine) -> PyResult<PyObjectRef> {
        self.try_not_released(vm).map(|_| self.buffer.obj.clone())
    }

    #[pygetset]
    fn nbytes(&self, vm: &VirtualMachine) -> PyResult<usize> {
        self.try_not_released(vm).map(|_| self.desc.len)
    }

    #[pygetset]
    fn readonly(&self, vm: &VirtualMachine) -> PyResult<bool> {
        self.try_not_released(vm).map(|_| self.desc.readonly)
    }

    #[pygetset]
    fn itemsize(&self, vm: &VirtualMachine) -> PyResult<usize> {
        self.try_not_released(vm).map(|_| self.desc.itemsize)
    }

    #[pygetset]
    fn ndim(&self, vm: &VirtualMachine) -> PyResult<usize> {
        self.try_not_released(vm).map(|_| self.desc.ndim())
    }

    #[pygetset]
    fn shape(&self, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        self.try_not_released(vm)?;
        Ok(vm.ctx.new_tuple(
            self.desc
                .dim_desc
                .iter()
                .map(|(shape, _, _)| shape.to_pyobject(vm))
                .collect(),
        ))
    }

    #[pygetset]
    fn strides(&self, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        self.try_not_released(vm)?;
        Ok(vm.ctx.new_tuple(
            self.desc
                .dim_desc
                .iter()
                .map(|(_, stride, _)| stride.to_pyobject(vm))
                .collect(),
        ))
    }

    #[pygetset]
    fn suboffsets(&self, vm: &VirtualMachine) -> PyResult<PyTupleRef> {
        self.try_not_released(vm)?;
        Ok(vm.ctx.new_tuple(
            self.desc
                .dim_desc
                .iter()
                .map(|(_, _, suboffset)| suboffset.to_pyobject(vm))
                .collect(),
        ))
    }

    #[pygetset]
    fn format(&self, vm: &VirtualMachine) -> PyResult<PyStr> {
        self.try_not_released(vm)
            .map(|_| PyStr::from(self.desc.format.clone()))
    }

    #[pygetset]
    fn contiguous(&self, vm: &VirtualMachine) -> PyResult<bool> {
        self.try_not_released(vm).map(|_| self.desc.is_contiguous())
    }

    #[pygetset]
    fn c_contiguous(&self, vm: &VirtualMachine) -> PyResult<bool> {
        self.try_not_released(vm).map(|_| self.desc.is_contiguous())
    }

    #[pygetset]
    fn f_contiguous(&self, vm: &VirtualMachine) -> PyResult<bool> {
        // TODO: column-major order
        self.try_not_released(vm)
            .map(|_| self.desc.ndim() <= 1 && self.desc.is_contiguous())
    }

    #[pymethod]
    fn __enter__(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult<PyRef<Self>> {
        zelf.try_not_released(vm).map(|_| zelf)
    }

    #[pymethod]
    fn __exit__(&self, _args: FuncArgs) {
        self.release();
    }

    #[pymethod]
    fn __getitem__(zelf: PyRef<Self>, needle: PyObjectRef, vm: &VirtualMachine) -> PyResult {
        zelf.try_not_released(vm)?;
        if zelf.desc.ndim() == 0 {
            // 0-d memoryview can be referenced using mv[...] or mv[()] only
            if needle.is(&vm.ctx.ellipsis) {
                return Ok(zelf.into());
            }
            if let Some(tuple) = needle.payload::<PyTuple>() {
                if tuple.is_empty() {
                    return zelf.unpack_single(0, vm);
                }
            }
            return Err(vm.new_type_error("invalid indexing of 0-dim memory"));
        }

        match SubscriptNeedle::try_from_object(vm, needle)? {
            SubscriptNeedle::Index(i) => zelf.getitem_by_idx(i, vm),
            SubscriptNeedle::Slice(slice) => zelf.getitem_by_slice(&slice, vm),
            SubscriptNeedle::MultiIndex(indices) => zelf.getitem_by_multi_idx(&indices, vm),
        }
    }

    #[pymethod]
    fn __delitem__(&self, _needle: PyObjectRef, vm: &VirtualMachine) -> PyResult<()> {
        if self.desc.readonly {
            return Err(vm.new_type_error("cannot modify read-only memory"));
        }
        Err(vm.new_type_error("cannot delete memory"))
    }

    #[pymethod]
    fn __len__(&self, vm: &VirtualMachine) -> PyResult<usize> {
        self.try_not_released(vm)?;
        Ok(if self.desc.ndim() == 0 {
            1
        } else {
            // shape for dim[0]
            self.desc.dim_desc[0].0
        })
    }

    #[pymethod]
    fn tobytes(&self, vm: &VirtualMachine) -> PyResult<PyBytesRef> {
        self.try_not_released(vm)?;
        let mut v = vec![];
        self.append_to(&mut v);
        Ok(PyBytes::from(v).into_ref(&vm.ctx))
    }

    #[pymethod]
    fn tolist(&self, vm: &VirtualMachine) -> PyResult<PyListRef> {
        self.try_not_released(vm)?;
        let bytes = self.buffer.obj_bytes();
        if self.desc.ndim() == 0 {
            return Ok(vm.ctx.new_list(vec![format_unpack(
                &self.format_spec,
                &bytes[..self.desc.itemsize],
                vm,
            )?]));
        }
        self._to_list(&bytes, 0, 0, vm)
    }

    #[pymethod]
    fn toreadonly(&self, vm: &VirtualMachine) -> PyResult<PyRef<Self>> {
        self.try_not_released(vm)?;
        let mut other = self.new_view();
        other.desc.readonly = true;
        Ok(other.into_ref(&vm.ctx))
    }

    #[pymethod]
    fn hex(
        &self,
        sep: OptionalArg<Either<PyStrRef, PyBytesRef>>,
        bytes_per_sep: OptionalArg<isize>,
        vm: &VirtualMachine,
    ) -> PyResult<String> {
        self.try_not_released(vm)?;
        self.contiguous_or_collect(|x| bytes_to_hex(x, sep, bytes_per_sep, vm))
    }

    fn cast_to_1d(&self, format: PyStrRef, vm: &VirtualMachine) -> PyResult<Self> {
        let format_spec = Self::parse_format(format.as_str(), vm)?;
        let itemsize = format_spec.size();
        if self.desc.len % itemsize != 0 {
            return Err(vm.new_type_error("memoryview: length is not a multiple of itemsize"));
        }

        Ok(Self {
            buffer: self.buffer.clone(),
            released: AtomicCell::new(false),
            start: self.start,
            format_spec,
            desc: BufferDescriptor {
                len: self.desc.len,
                readonly: self.desc.readonly,
                itemsize,
                format: format.to_string().into(),
                dim_desc: vec![(self.desc.len / itemsize, itemsize as isize, 0)],
            },
            hash: OnceCell::new(),
        })
    }

    #[pymethod]
    fn cast(&self, args: CastArgs, vm: &VirtualMachine) -> PyResult<PyRef<Self>> {
        self.try_not_released(vm)?;
        if !self.desc.is_contiguous() {
            return Err(vm.new_type_error("memoryview: casts are restricted to C-contiguous views"));
        }

        let CastArgs { format, shape } = args;

        if let OptionalArg::Present(shape) = shape {
            if self.desc.is_zero_in_shape() {
                return Err(vm.new_type_error(
                    "memoryview: cannot cast view with zeros in shape or strides",
                ));
            }

            let tup;
            let list;
            let list_borrow;
            let shape = match shape {
                Either::A(shape) => {
                    tup = shape;
                    tup.as_slice()
                }
                Either::B(shape) => {
                    list = shape;
                    list_borrow = list.borrow_vec();
                    &list_borrow
                }
            };

            let shape_ndim = shape.len();
            // TODO: MAX_NDIM
            if self.desc.ndim() != 1 && shape_ndim != 1 {
                return Err(vm.new_type_error("memoryview: cast must be 1D -> ND or ND -> 1D"));
            }

            let mut other = self.cast_to_1d(format, vm)?;
            let itemsize = other.desc.itemsize;

            // 0 ndim is single item
            if shape_ndim == 0 {
                other.desc.dim_desc = vec![];
                other.desc.len = itemsize;
                return Ok(other.into_ref(&vm.ctx));
            }

            let mut product_shape = itemsize;
            let mut dim_descriptor = Vec::with_capacity(shape_ndim);

            for x in shape {
                let x = usize::try_from_borrowed_object(vm, x)?;

                if x > isize::MAX as usize / product_shape {
                    return Err(vm.new_value_error("memoryview.cast(): product(shape) > SSIZE_MAX"));
                }
                product_shape *= x;
                dim_descriptor.push((x, 0, 0));
            }

            dim_descriptor.last_mut().unwrap().1 = itemsize as isize;
            for i in (0..dim_descriptor.len() - 1).rev() {
                dim_descriptor[i].1 = dim_descriptor[i + 1].1 * dim_descriptor[i + 1].0 as isize;
            }

            if product_shape != other.desc.len {
                return Err(
                    vm.new_type_error("memoryview: product(shape) * itemsize != buffer size")
                );
            }

            other.desc.dim_desc = dim_descriptor;

            Ok(other.into_ref(&vm.ctx))
        } else {
            Ok(self.cast_to_1d(format, vm)?.into_ref(&vm.ctx))
        }
    }
}

#[pyclass]
impl Py<PyMemoryView> {
    #[pymethod]
    fn __setitem__(
        &self,
        needle: PyObjectRef,
        value: PyObjectRef,
        vm: &VirtualMachine,
    ) -> PyResult<()> {
        self.try_not_released(vm)?;
        if self.desc.readonly {
            return Err(vm.new_type_error("cannot modify read-only memory"));
        }
        if value.is(&vm.ctx.none) {
            return Err(vm.new_type_error("cannot delete memory"));
        }

        if self.desc.ndim() == 0 {
            // TODO: merge branches when we got conditional if let
            if needle.is(&vm.ctx.ellipsis) {
                return self.pack_single(0, value, vm);
            } else if let Some(tuple) = needle.payload::<PyTuple>() {
                if tuple.is_empty() {
                    return self.pack_single(0, value, vm);
                }
            }
            return Err(vm.new_type_error("invalid indexing of 0-dim memory"));
        }
        match SubscriptNeedle::try_from_object(vm, needle)? {
            SubscriptNeedle::Index(i) => self.setitem_by_idx(i, value, vm),
            SubscriptNeedle::Slice(slice) => self.setitem_by_slice(&slice, value, vm),
            SubscriptNeedle::MultiIndex(indices) => self.setitem_by_multi_idx(&indices, value, vm),
        }
    }

    #[pymethod]
    fn __reduce_ex__(&self, _proto: usize, vm: &VirtualMachine) -> PyResult {
        self.__reduce__(vm)
    }

    #[pymethod]
    fn __reduce__(&self, vm: &VirtualMachine) -> PyResult {
        Err(vm.new_type_error("cannot pickle 'memoryview' object"))
    }
}

#[derive(FromArgs)]
struct CastArgs {
    #[pyarg(any)]
    format: PyStrRef,
    #[pyarg(any, optional)]
    shape: OptionalArg<Either<PyTupleRef, PyListRef>>,
}

enum SubscriptNeedle {
    Index(isize),
    Slice(PyRef<PySlice>),
    MultiIndex(Vec<isize>),
    // MultiSlice(Vec<PySliceRef>),
}

impl TryFromObject for SubscriptNeedle {
    fn try_from_object(vm: &VirtualMachine, obj: PyObjectRef) -> PyResult<Self> {
        // TODO: number protocol
        if let Some(i) = obj.payload::<PyInt>() {
            Ok(Self::Index(i.try_to_primitive(vm)?))
        } else if obj.payload_is::<PySlice>() {
            Ok(Self::Slice(unsafe { obj.downcast_unchecked::<PySlice>() }))
        } else if let Ok(i) = obj.try_index(vm) {
            Ok(Self::Index(i.try_to_primitive(vm)?))
        } else {
            if let Some(tuple) = obj.payload::<PyTuple>() {
                if tuple.iter().all(|x| x.payload_is::<PyInt>()) {
                    let v = tuple
                        .iter()
                        .map(|x| {
                            unsafe { x.downcast_unchecked_ref::<PyInt>() }
                                .try_to_primitive::<isize>(vm)
                        })
                        .try_collect()?;
                    return Ok(Self::MultiIndex(v));
                } else if tuple.iter().all(|x| x.payload_is::<PySlice>()) {
                    return Err(vm.new_not_implemented_error(
                        "multi-dimensional slicing is not implemented",
                    ));
                }
            }
            Err(vm.new_type_error("memoryview: invalid slice key"))
        }
    }
}

static BUFFER_METHODS: BufferMethods = BufferMethods {
    obj_bytes: |buffer| buffer.obj_as::<PyMemoryView>().obj_bytes(),
    obj_bytes_mut: |buffer| buffer.obj_as::<PyMemoryView>().obj_bytes_mut(),
    release: |buffer| buffer.obj_as::<PyMemoryView>().buffer.release(),
    retain: |buffer| buffer.obj_as::<PyMemoryView>().buffer.retain(),
};

impl AsBuffer for PyMemoryView {
    fn as_buffer(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyBuffer> {
        if zelf.released.load() {
            Err(vm.new_value_error("operation forbidden on released memoryview object"))
        } else {
            Ok(PyBuffer::new(
                zelf.to_owned().into(),
                zelf.desc.clone(),
                &BUFFER_METHODS,
            ))
        }
    }
}

impl Drop for PyMemoryView {
    fn drop(&mut self) {
        if self.released.load() {
            unsafe { self.buffer.drop_without_release() };
        } else {
            unsafe { ManuallyDrop::drop(&mut self.buffer) };
        }
    }
}

impl AsMapping for PyMemoryView {
    fn as_mapping() -> &'static PyMappingMethods {
        static AS_MAPPING: PyMappingMethods = PyMappingMethods {
            length: atomic_func!(|mapping, vm| PyMemoryView::mapping_downcast(mapping).__len__(vm)),
            subscript: atomic_func!(|mapping, needle, vm| {
                let zelf = PyMemoryView::mapping_downcast(mapping);
                PyMemoryView::__getitem__(zelf.to_owned(), needle.to_owned(), vm)
            }),
            ass_subscript: atomic_func!(|mapping, needle, value, vm| {
                let zelf = PyMemoryView::mapping_downcast(mapping);
                if let Some(value) = value {
                    zelf.__setitem__(needle.to_owned(), value, vm)
                } else {
                    Err(vm.new_type_error("cannot delete memory".to_owned()))
                }
            }),
        };
        &AS_MAPPING
    }
}

impl AsSequence for PyMemoryView {
    fn as_sequence() -> &'static PySequenceMethods {
        static AS_SEQUENCE: LazyLock<PySequenceMethods> = LazyLock::new(|| PySequenceMethods {
            length: atomic_func!(|seq, vm| {
                let zelf = PyMemoryView::sequence_downcast(seq);
                zelf.try_not_released(vm)?;
                zelf.__len__(vm)
            }),
            item: atomic_func!(|seq, i, vm| {
                let zelf = PyMemoryView::sequence_downcast(seq);
                zelf.try_not_released(vm)?;
                zelf.getitem_by_idx(i, vm)
            }),
            ..PySequenceMethods::NOT_IMPLEMENTED
        });
        &AS_SEQUENCE
    }
}

impl Comparable for PyMemoryView {
    fn cmp(
        zelf: &Py<Self>,
        other: &PyObject,
        op: PyComparisonOp,
        vm: &VirtualMachine,
    ) -> PyResult<PyComparisonValue> {
        match op {
            PyComparisonOp::Ne => {
                Self::eq(zelf, other, vm).map(|x| PyComparisonValue::Implemented(!x))
            }
            PyComparisonOp::Eq => Self::eq(zelf, other, vm).map(PyComparisonValue::Implemented),
            _ => Err(vm.new_type_error(format!(
                "'{}' not supported between instances of '{}' and '{}'",
                op.operator_token(),
                zelf.class().name(),
                other.class().name()
            ))),
        }
    }
}

impl Hashable for PyMemoryView {
    fn hash(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyHash> {
        zelf.hash
            .get_or_try_init(|| {
                zelf.try_not_released(vm)?;
                if !zelf.desc.readonly {
                    return Err(vm.new_value_error("cannot hash writable memoryview object"));
                }
                Ok(zelf.contiguous_or_collect(|bytes| vm.state.hash_secret.hash_bytes(bytes)))
            })
            .copied()
    }
}

impl PyPayload for PyMemoryView {
    #[inline]
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.memoryview_type
    }
}

impl Representable for PyMemoryView {
    #[inline]
    fn repr_str(zelf: &Py<Self>, _vm: &VirtualMachine) -> PyResult<String> {
        let repr = if zelf.released.load() {
            format!("<released memory at {:#x}>", zelf.get_id())
        } else {
            format!("<memory at {:#x}>", zelf.get_id())
        };
        Ok(repr)
    }
}

pub(crate) fn init(ctx: &Context) {
    PyMemoryView::extend_class(ctx, ctx.types.memoryview_type);
    PyMemoryViewIterator::extend_class(ctx, ctx.types.memoryviewiterator_type);
}

fn format_unpack(
    format_spec: &FormatSpec,
    bytes: &[u8],
    vm: &VirtualMachine,
) -> PyResult<PyObjectRef> {
    format_spec.unpack(bytes, vm).map(|x| {
        if x.len() == 1 {
            x[0].to_owned()
        } else {
            x.into()
        }
    })
}

fn is_equiv_shape(a: &BufferDescriptor, b: &BufferDescriptor) -> bool {
    if a.ndim() != b.ndim() {
        return false;
    }

    let a_iter = a.dim_desc.iter().map(|x| x.0);
    let b_iter = b.dim_desc.iter().map(|x| x.0);
    for (a_shape, b_shape) in a_iter.zip(b_iter) {
        if a_shape != b_shape {
            return false;
        }
        // if both shape is 0, ignore the rest
        if a_shape == 0 {
            break;
        }
    }
    true
}

fn is_equiv_format(a: &BufferDescriptor, b: &BufferDescriptor) -> bool {
    // TODO: skip @
    a.itemsize == b.itemsize && a.format == b.format
}

fn is_equiv_structure(a: &BufferDescriptor, b: &BufferDescriptor) -> bool {
    is_equiv_format(a, b) && is_equiv_shape(a, b)
}

impl Iterable for PyMemoryView {
    fn iter(zelf: PyRef<Self>, vm: &VirtualMachine) -> PyResult {
        Ok(PyMemoryViewIterator {
            internal: PyMutex::new(PositionIterInternal::new(zelf, 0)),
        }
        .into_pyobject(vm))
    }
}

#[pyclass(module = false, name = "memory_iterator")]
#[derive(Debug, Traverse)]
pub struct PyMemoryViewIterator {
    internal: PyMutex<PositionIterInternal<PyRef<PyMemoryView>>>,
}

impl PyPayload for PyMemoryViewIterator {
    fn class(ctx: &Context) -> &'static Py<PyType> {
        ctx.types.memoryviewiterator_type
    }
}

#[pyclass(with(Unconstructible, IterNext, Iterable))]
impl PyMemoryViewIterator {
    #[pymethod]
    fn __reduce__(&self, vm: &VirtualMachine) -> PyTupleRef {
        self.internal
            .lock()
            .builtins_iter_reduce(|x| x.clone().into(), vm)
    }
}
impl Unconstructible for PyMemoryViewIterator {}

impl SelfIter for PyMemoryViewIterator {}
impl IterNext for PyMemoryViewIterator {
    fn next(zelf: &Py<Self>, vm: &VirtualMachine) -> PyResult<PyIterReturn> {
        zelf.internal.lock().next(|mv, pos| {
            let len = mv.__len__(vm)?;
            Ok(if pos >= len {
                PyIterReturn::StopIteration(None)
            } else {
                PyIterReturn::Return(mv.getitem_by_idx(pos.try_into().unwrap(), vm)?)
            })
        })
    }
}
