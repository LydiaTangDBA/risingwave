// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! An owned row type with a `Vec<Datum>`.

use std::ops;

use super::{Row2, RowExt};
use crate::collection::estimate_size::EstimateSize;
use crate::types::{DataType, Datum, DatumRef, ToDatumRef};
use crate::util::ordered::OrderedRowSerde;
use crate::util::value_encoding;
use crate::util::value_encoding::deserialize_datum;

/// TODO(row trait): rename to `OwnedRow`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Row(Vec<Datum>); // made private to avoid abuse

/// Do not implement `IndexMut` to make it immutable.
impl ops::Index<usize> for Row {
    type Output = Datum;

    fn index(&self, index: usize) -> &Self::Output {
        &self.0[index]
    }
}

impl PartialOrd for Row {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        if self.0.len() != other.0.len() {
            return None;
        }
        self.0.partial_cmp(&other.0)
    }
}

impl Ord for Row {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.partial_cmp(other).unwrap_or_else(|| {
            panic!("cannot compare rows with different lengths:\n left: {self:?}\nright: {other:?}")
        })
    }
}

impl Row {
    pub fn new(values: Vec<Datum>) -> Self {
        Self(values)
    }

    /// Retrieve the underlying [`Vec<Datum>`].
    pub fn into_inner(self) -> Vec<Datum> {
        self.0
    }

    /// Returns a reference to an empty row.
    ///
    /// Note: use [`empty`](super::empty) if possible.
    pub fn empty<'a>() -> &'a Self {
        static EMPTY_ROW: Row = Row(Vec::new());
        &EMPTY_ROW
    }

    /// Serialize part of the row into memcomparable bytes.
    ///
    /// TODO(row trait): introduce `Row::memcmp_serialize`.
    pub fn extract_memcomparable_by_indices(
        &self,
        serializer: &OrderedRowSerde,
        key_indices: &[usize],
    ) -> Vec<u8> {
        let mut bytes = vec![];
        serializer.serialize((&self).project(key_indices), &mut bytes);
        bytes
    }
}

impl EstimateSize for Row {
    fn estimated_heap_size(&self) -> usize {
        // FIXME(bugen): this is not accurate now as the heap size of some `Scalar` is not counted.
        self.0.capacity() * std::mem::size_of::<Datum>()
    }
}

impl Row2 for Row {
    type Iter<'a> = impl Iterator<Item = DatumRef<'a>>
    where
        Self: 'a;

    #[inline]
    fn datum_at(&self, index: usize) -> DatumRef<'_> {
        self[index].to_datum_ref()
    }

    #[inline]
    unsafe fn datum_at_unchecked(&self, index: usize) -> DatumRef<'_> {
        self.0.get_unchecked(index).to_datum_ref()
    }

    #[inline]
    fn len(&self) -> usize {
        self.0.len()
    }

    #[inline]
    fn iter(&self) -> Self::Iter<'_> {
        Iterator::map(self.0.iter(), ToDatumRef::to_datum_ref)
    }

    #[inline]
    fn to_owned_row(&self) -> Row {
        self.clone()
    }

    #[inline]
    fn into_owned_row(self) -> Row {
        self
    }
}

/// Deserializer of the `Row`.
#[derive(Clone, Debug)]
pub struct RowDeserializer<D: AsRef<[DataType]> = Vec<DataType>> {
    data_types: D,
}

impl<D: AsRef<[DataType]>> RowDeserializer<D> {
    /// Creates a new `RowDeserializer` with row schema.
    pub fn new(data_types: D) -> Self {
        RowDeserializer { data_types }
    }

    /// Deserialize the row from value encoding bytes.
    pub fn deserialize(&self, mut data: impl bytes::Buf) -> value_encoding::Result<Row> {
        let mut values = Vec::with_capacity(self.data_types().len());
        for typ in self.data_types() {
            values.push(deserialize_datum(&mut data, typ)?);
        }
        Ok(Row(values))
    }

    pub fn data_types(&self) -> &[DataType] {
        self.data_types.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use itertools::Itertools;

    use super::*;
    use crate::types::{DataType as Ty, IntervalUnit, ScalarImpl};
    use crate::util::hash_util::Crc32FastBuilder;

    #[test]
    fn row_value_encode_decode() {
        let row = Row::new(vec![
            Some(ScalarImpl::Utf8("string".into())),
            Some(ScalarImpl::Bool(true)),
            Some(ScalarImpl::Int16(1)),
            Some(ScalarImpl::Int32(2)),
            Some(ScalarImpl::Int64(3)),
            Some(ScalarImpl::Float32(4.0.into())),
            Some(ScalarImpl::Float64(5.0.into())),
            Some(ScalarImpl::Decimal("-233.3".parse().unwrap())),
            Some(ScalarImpl::Interval(IntervalUnit::new(7, 8, 9))),
        ]);
        let value_indices = (0..9).collect_vec();
        let bytes = (&row).project(&value_indices).value_serialize();
        assert_eq!(bytes.len(), 10 + 1 + 2 + 4 + 8 + 4 + 8 + 16 + 16 + 9);
        let de = RowDeserializer::new(vec![
            Ty::Varchar,
            Ty::Boolean,
            Ty::Int16,
            Ty::Int32,
            Ty::Int64,
            Ty::Float32,
            Ty::Float64,
            Ty::Decimal,
            Ty::Interval,
        ]);
        let row1 = de.deserialize(bytes.as_ref()).unwrap();
        assert_eq!(row, row1);
    }

    #[test]
    fn test_hash_row() {
        let hash_builder = Crc32FastBuilder;

        let row1 = Row::new(vec![
            Some(ScalarImpl::Utf8("string".into())),
            Some(ScalarImpl::Bool(true)),
            Some(ScalarImpl::Int16(1)),
            Some(ScalarImpl::Int32(2)),
            Some(ScalarImpl::Int64(3)),
            Some(ScalarImpl::Float32(4.0.into())),
            Some(ScalarImpl::Float64(5.0.into())),
            Some(ScalarImpl::Decimal("-233.3".parse().unwrap())),
            Some(ScalarImpl::Interval(IntervalUnit::new(7, 8, 9))),
        ]);
        let row2 = Row::new(vec![
            Some(ScalarImpl::Interval(IntervalUnit::new(7, 8, 9))),
            Some(ScalarImpl::Utf8("string".into())),
            Some(ScalarImpl::Bool(true)),
            Some(ScalarImpl::Int16(1)),
            Some(ScalarImpl::Int32(2)),
            Some(ScalarImpl::Int64(3)),
            Some(ScalarImpl::Float32(4.0.into())),
            Some(ScalarImpl::Float64(5.0.into())),
            Some(ScalarImpl::Decimal("-233.3".parse().unwrap())),
        ]);
        assert_ne!(row1.hash(hash_builder), row2.hash(hash_builder));

        let row_default = Row::default();
        assert_eq!(row_default.hash(hash_builder).0, 0);
    }
}
