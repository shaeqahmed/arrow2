use std::sync::Arc;

use crate::{
    bitmap::Bitmap,
    datatypes::{DataType, Field},
    error::Error,
    offset::{Offset, Offsets, OffsetsBuffer},
};

use super::{new_empty_array, specification::try_check_offsets_bounds, Array, PrimitiveArray};

#[cfg(feature = "arrow")]
mod data;
mod ffi;
pub(super) mod fmt;
mod iterator;
pub use iterator::*;
mod mutable;
pub use mutable::*;

/// An [`Array`] semantically equivalent to `Vec<Option<Vec<Option<T>>>>` with Arrow's in-memory.
#[derive(Clone)]
pub struct ListArray<O: Offset> {
    data_type: DataType,
    offsets: OffsetsBuffer<O>,
    values: Box<dyn Array>,
    validity: Option<Bitmap>,
}

impl<O: Offset> ListArray<O> {
    /// Creates a new [`ListArray`].
    ///
    /// # Errors
    /// This function returns an error iff:
    /// * The last offset is not equal to the values' length.
    /// * the validity's length is not equal to `offsets.len()`.
    /// * The `data_type`'s [`crate::datatypes::PhysicalType`] is not equal to either [`crate::datatypes::PhysicalType::List`] or [`crate::datatypes::PhysicalType::LargeList`].
    /// * The `data_type`'s inner field's data type is not equal to `values.data_type`.
    /// # Implementation
    /// This function is `O(1)`
    pub fn try_new(
        data_type: DataType,
        offsets: OffsetsBuffer<O>,
        values: Box<dyn Array>,
        validity: Option<Bitmap>,
    ) -> Result<Self, Error> {
        try_check_offsets_bounds(&offsets, values.len())?;

        if validity
            .as_ref()
            .map_or(false, |validity| validity.len() != offsets.len_proxy())
        {
            return Err(Error::oos(
                "validity mask length must match the number of values",
            ));
        }

        let child_data_type = Self::try_get_child(&data_type)?.data_type();
        let values_data_type = values.data_type();
        if child_data_type != values_data_type {
            return Err(Error::oos(
                format!("ListArray's child's DataType must match. However, the expected DataType is {child_data_type:?} while it got {values_data_type:?}."),
            ));
        }

        Ok(Self {
            data_type,
            offsets,
            values,
            validity,
        })
    }

    /// Creates a new [`ListArray`].
    ///
    /// # Panics
    /// This function panics iff:
    /// * The last offset is not equal to the values' length.
    /// * the validity's length is not equal to `offsets.len()`.
    /// * The `data_type`'s [`crate::datatypes::PhysicalType`] is not equal to either [`crate::datatypes::PhysicalType::List`] or [`crate::datatypes::PhysicalType::LargeList`].
    /// * The `data_type`'s inner field's data type is not equal to `values.data_type`.
    /// # Implementation
    /// This function is `O(1)`
    pub fn new(
        data_type: DataType,
        offsets: OffsetsBuffer<O>,
        values: Box<dyn Array>,
        validity: Option<Bitmap>,
    ) -> Self {
        Self::try_new(data_type, offsets, values, validity).unwrap()
    }

    /// Returns a new empty [`ListArray`].
    pub fn new_empty(data_type: DataType) -> Self {
        let values = new_empty_array(Self::get_child_type(&data_type).clone());
        Self::new(data_type, OffsetsBuffer::default(), values, None)
    }

    /// Returns a new null [`ListArray`].
    #[inline]
    pub fn new_null(data_type: DataType, length: usize) -> Self {
        let child = Self::get_child_type(&data_type).clone();
        Self::new(
            data_type,
            Offsets::new_zeroed(length).into(),
            new_empty_array(child),
            Some(Bitmap::new_zeroed(length)),
        )
    }
}

impl<O: Offset> ListArray<O> {
    /// Slices this [`ListArray`].
    /// # Panics
    /// panics iff `offset + length >= self.len()`
    pub fn slice(&mut self, offset: usize, length: usize) {
        assert!(
            offset + length <= self.len(),
            "the offset of the new Buffer cannot exceed the existing length"
        );
        unsafe { self.slice_unchecked(offset, length) }
    }

    /// Slices this [`ListArray`].
    /// # Safety
    /// The caller must ensure that `offset + length < self.len()`.
    pub unsafe fn slice_unchecked(&mut self, offset: usize, length: usize) {
        self.validity.as_mut().and_then(|bitmap| {
            bitmap.slice_unchecked(offset, length);
            (bitmap.unset_bits() > 0).then(|| bitmap)
        });
        self.offsets.slice_unchecked(offset, length + 1);
    }

    impl_sliced!();
    impl_mut_validity!();
    impl_into_array!();
}

// Accessors
impl<O: Offset> ListArray<O> {
    /// Returns the length of this array
    #[inline]
    pub fn len(&self) -> usize {
        self.offsets.len_proxy()
    }

    /// Returns the element at index `i`
    /// # Panic
    /// Panics iff `i >= self.len()`
    #[inline]
    pub fn value(&self, i: usize) -> Box<dyn Array> {
        assert!(i < self.len());
        // Safety: invariant of this function
        unsafe { self.value_unchecked(i) }
    }

    /// Returns the element at index `i` as &str
    /// # Safety
    /// Assumes that the `i < self.len`.
    #[inline]
    pub unsafe fn value_unchecked(&self, i: usize) -> Box<dyn Array> {
        // safety: the invariant of the function
        let (start, end) = self.offsets.start_end_unchecked(i);
        let length = end - start;

        // safety: the invariant of the struct
        self.values.sliced_unchecked(start, length)
    }

    /// The optional validity.
    #[inline]
    pub fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    /// The offsets [`Buffer`].
    #[inline]
    pub fn offsets(&self) -> &OffsetsBuffer<O> {
        &self.offsets
    }

    /// The values.
    #[inline]
    pub fn values(&self) -> &Box<dyn Array> {
        &self.values
    }
}

impl<O: Offset> ListArray<O> {
    /// Returns a default [`DataType`]: inner field is named "item" and is nullable
    pub fn default_datatype(data_type: DataType) -> DataType {
        let field = Arc::new(Field::new("item", data_type, true));
        if O::IS_LARGE {
            DataType::LargeList(field)
        } else {
            DataType::List(field)
        }
    }

    /// Returns a the inner [`Field`]
    /// # Panics
    /// Panics iff the logical type is not consistent with this struct.
    pub fn get_child_field(data_type: &DataType) -> &Field {
        Self::try_get_child(data_type).unwrap()
    }

    /// Returns a the inner [`Field`]
    /// # Errors
    /// Panics iff the logical type is not consistent with this struct.
    pub fn try_get_child(data_type: &DataType) -> Result<&Field, Error> {
        if O::IS_LARGE {
            match data_type.to_logical_type() {
                DataType::LargeList(child) => Ok(child.as_ref()),
                _ => Err(Error::oos("ListArray<i64> expects DataType::LargeList")),
            }
        } else {
            match data_type.to_logical_type() {
                DataType::List(child) => Ok(child.as_ref()),
                _ => Err(Error::oos("ListArray<i32> expects DataType::List")),
            }
        }
    }

    /// Returns a the inner [`DataType`]
    /// # Panics
    /// Panics iff the logical type is not consistent with this struct.
    pub fn get_child_type(data_type: &DataType) -> &DataType {
        Self::get_child_field(data_type).data_type()
    }
}

impl<O: Offset> Array for ListArray<O> {
    impl_common_array!();

    fn validity(&self) -> Option<&Bitmap> {
        self.validity.as_ref()
    }

    #[inline]
    fn with_validity(&self, validity: Option<Bitmap>) -> Box<dyn Array> {
        Box::new(self.clone().with_validity(validity))
    }
}

/// arrow2 -> arrow1 conversion
#[cfg(feature = "arrow")]
impl<O: Offset + arrow_array::OffsetSizeTrait> From<ListArray<O>>
    for arrow_array::GenericListArray<O>
{
    fn from(value: ListArray<O>) -> Self {
        let field = ListArray::<O>::get_child_field(value.data_type());
        let field = Arc::new(arrow_schema::Field::new(
            "item",
            field.data_type.clone().into(),
            field.is_nullable,
        ));
        let offsets = value.offsets().clone().into();
        let values = value.values().clone().into();
        let nulls = value.validity().map(|x| x.clone().into());
        Self::new(field, offsets, values, nulls)
    }
}

/// arrow1 -> arrow2 conversion
#[cfg(feature = "arrow")]
impl<O: Offset + arrow_array::OffsetSizeTrait> From<arrow_array::GenericListArray<O>>
    for ListArray<O>
{
    fn from(array1: arrow_array::GenericListArray<O>) -> Self {
        let (field1, offset_buffer1, array1, nulls1) = array1.into_parts();

        let field2 = Arc::new(Field::from(arrow_schema::Field::clone(&field1)));
        let data_type2 = if <O as Offset>::IS_LARGE {
            DataType::LargeList(field2)
        } else {
            DataType::List(field2)
        };

        Self::new(
            data_type2,
            offset_buffer1.into(),
            array1.into(),
            nulls1.map(Bitmap::from_arrow),
        )
    }
}

#[cfg(feature = "arrow")]
#[test]
fn test_arrow_list_array_conversion_non_null() {
    #![allow(clippy::zero_prefixed_literal)]

    for inner_nullability in [false, true] {
        /*
            We build this:

            [0_001, 0_002],
            [1_001, 1_002, 1_003],
            [],
            [3_001, 3_002],
            [4_001],
        */

        use arrow_array::Array;
        let offsets =
            OffsetsBuffer::<i32>::from(Offsets::try_from(vec![0, 2, 5, 5, 7, 8]).unwrap());
        let values = PrimitiveArray::<i16>::from_vec(vec![
            0_001_i16, 0_002, //
            1_001, 1_002, 1_003, //
            //
            3_001, 3_002, //
            4_001,
        ]);

        let bitmap = None;
        let list_array = ListArray::new(
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Int16,
                inner_nullability,
            ))),
            offsets,
            values.boxed(),
            bitmap,
        );

        // Skip first and last elements:
        let list_array = list_array.sliced(1, 3);
        assert!(list_array.validity().is_none());

        assert_eq!(list_array.len(), 3);
        assert_eq!(list_array.value(0).len(), 3);
        assert_eq!(list_array.value(1).len(), 0);
        assert_eq!(list_array.value(2).len(), 2);

        let list_array_1 = arrow_array::ListArray::from(list_array.clone());
        assert!(list_array_1.nulls().is_none());
        assert_eq!(list_array_1.value_length(0), 3);
        assert_eq!(list_array_1.value_length(1), 0);
        assert_eq!(list_array_1.value_length(2), 2);

        let roundtripped = ListArray::from(list_array_1);
        assert!(roundtripped.validity().is_none());

        assert_eq!(list_array.data_type(), roundtripped.data_type());
        assert_eq!(list_array, roundtripped);
    }
}

#[cfg(feature = "arrow")]
#[test]
fn test_arrow_list_array_conversion_nullable() {
    #![allow(clippy::zero_prefixed_literal)]
    use arrow_array::Array as _;

    for inner_nullability in [false, true] {
        /*
        We build this:

        [0_001, 0_002],
        [1_001, 1_002, 1_003],
        [],
        [3_001, 3_002],
        null,
        [4_001],
         */
        let offsets =
            OffsetsBuffer::<i32>::from(Offsets::try_from(vec![0, 2, 5, 5, 7, 7, 8]).unwrap());
        let values = PrimitiveArray::<i16>::from_vec(vec![
            0_001_i16, 0_002, //
            1_001, 1_002, 1_003, //
            // []
            3_001, 3_002, //
            // null
            4_001,
        ]);
        let bitmap = Some(Bitmap::from([true, true, true, true, false, true]));

        let list_array = ListArray::new(
            DataType::List(Arc::new(Field::new(
                "item",
                DataType::Int16,
                inner_nullability,
            ))),
            offsets,
            values.boxed(),
            bitmap,
        );

        // Skip first and last elements:
        let list_array = list_array.sliced(1, 4);

        assert_eq!(list_array.len(), 4);
        assert_eq!(list_array.value(0).len(), 3);
        assert_eq!(list_array.value(1).len(), 0);
        assert_eq!(list_array.value(2).len(), 2);
        assert_eq!(list_array.value(3).len(), 0); // null

        let list_array_1 = arrow_array::ListArray::from(list_array.clone());
        assert!(list_array_1.nulls().is_some());
        assert_eq!(list_array_1.value_length(0), 3);
        assert_eq!(list_array_1.value_length(1), 0);
        assert_eq!(list_array_1.value_length(2), 2);
        assert_eq!(list_array_1.value_length(3), 0); // null

        let roundtripped = ListArray::from(list_array_1);

        assert_eq!(list_array.data_type(), roundtripped.data_type());
        assert_eq!(list_array, roundtripped);
        assert!(roundtripped.validity().is_some());
    }
}
