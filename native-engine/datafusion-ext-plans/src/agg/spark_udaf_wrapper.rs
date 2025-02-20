// Copyright 2022 The Blaze Authors
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::{
    any::Any,
    fmt::{Debug, Display, Formatter},
    io::Cursor,
    sync::Arc,
};
use std::io::Write;

use arrow::{
    array::{
        as_struct_array, make_array, Array, ArrayRef, AsArray, BinaryArray, Int32Array,
        Int32Builder, StructArray,
    },
    datatypes::{DataType, Field, Schema, SchemaRef},
    ffi::{from_ffi, FFI_ArrowArray, FFI_ArrowSchema},
    record_batch::{RecordBatch, RecordBatchOptions},
};
use arrow_schema::FieldRef;
use blaze_jni_bridge::{jni_call, jni_new_direct_byte_buffer, jni_new_global_ref, jni_new_object};
use datafusion::{common::Result, physical_expr::PhysicalExpr};
use datafusion_ext_commons::{
    downcast_any,
    io::{read_len, write_len},
};
use jni::objects::{GlobalRef, JObject};
use once_cell::sync::OnceCell;
use datafusion_ext_commons::io::read_bytes_into_vec;

use crate::{
    agg::{
        acc::{AccColumn, AccColumnRef},
        agg::{Agg, IdxSelection},
    },
    idx_for_zipped,
    memmgr::spill::{SpillCompressedReader, SpillCompressedWriter},
};

pub struct SparkUDAFWrapper {
    serialized: Vec<u8>,
    pub return_type: DataType,
    child: Vec<Arc<dyn PhysicalExpr>>,
    import_schema: SchemaRef,
    params_schema: OnceCell<SchemaRef>,
    jcontext: OnceCell<GlobalRef>,
}

impl SparkUDAFWrapper {
    pub fn try_new(
        serialized: Vec<u8>,
        return_type: DataType,
        child: Vec<Arc<dyn PhysicalExpr>>,
    ) -> Result<Self> {
        Ok(Self {
            serialized,
            return_type: return_type.clone(),
            child,
            import_schema: Arc::new(Schema::new(vec![Field::new("", return_type, true)])),
            params_schema: OnceCell::new(),
            jcontext: OnceCell::new(),
        })
    }

    fn jcontext(&self) -> Result<GlobalRef> {
        self.jcontext
            .get_or_try_init(|| {
                let serialized_buf = jni_new_direct_byte_buffer!(&self.serialized)?;
                let jcontext_local =
                    jni_new_object!(SparkUDAFWrapperContext(serialized_buf.as_obj()))?;
                jni_new_global_ref!(jcontext_local.as_obj())
            })
            .cloned()
    }
}

impl Display for SparkUDAFWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SparkUDAFWrapper")
    }
}

impl Debug for SparkUDAFWrapper {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "SparkUDAFWrapper({:?})", self.child)
    }
}

impl Agg for SparkUDAFWrapper {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn exprs(&self) -> Vec<Arc<dyn PhysicalExpr>> {
        self.child.clone()
    }

    fn data_type(&self) -> &DataType {
        &self.return_type
    }

    fn nullable(&self) -> bool {
        true
    }

    fn create_acc_column(&self, num_rows: usize) -> AccColumnRef {
        let jcontext = self.jcontext().unwrap();
        let rows = jni_call!(SparkUDAFWrapperContext(jcontext.as_obj()).initialize(
            num_rows as i32,
        )-> JObject)
        .unwrap();

        let jcontext = self.jcontext().unwrap();
        let obj = jni_new_global_ref!(rows.as_obj()).unwrap();
        Box::new(AccUnsafeRowsColumn {
            obj,
            jcontext,
            num_rows,
        })
    }

    fn with_new_exprs(&self, _exprs: Vec<Arc<dyn PhysicalExpr>>) -> Result<Arc<dyn Agg>> {
        Ok(Arc::new(Self::try_new(
            self.serialized.clone(),
            self.return_type.clone(),
            self.child.clone(),
        )?))
    }

    // todo: implemented prepare_partial_args
    // fn prepare_partial_args(&self, partial_inputs: &[ArrayRef]) ->
    // Result<Vec<ArrayRef>> {     // cast arg1 to target data type
    //     Ok(vec![datafusion_ext_commons::arrow::cast::cast(
    //         &partial_inputs[0],
    //         &self.return_type,
    //     )?])
    // }

    fn partial_update(
        &self,
        accs: &mut AccColumnRef,
        acc_idx: IdxSelection<'_>,
        partial_args: &[ArrayRef],
        partial_arg_idx: IdxSelection<'_>,
        batch_schema: SchemaRef,
    ) -> Result<()> {
        let accs = downcast_any!(accs, mut AccUnsafeRowsColumn).unwrap();

        let params = partial_args.to_vec();
        let params_schema = self
            .params_schema
            .get_or_try_init(|| -> Result<SchemaRef> {
                let mut param_fields = Vec::with_capacity(self.child.len());
                for child in &self.child {
                    param_fields.push(Field::new(
                        "",
                        child.data_type(batch_schema.as_ref())?,
                        child.nullable(batch_schema.as_ref())?,
                    ));
                }
                Ok(Arc::new(Schema::new(param_fields)))
            })?;
        let params_batch = RecordBatch::try_new_with_options(
            params_schema.clone(),
            params.clone(),
            &RecordBatchOptions::new().with_row_count(Some(partial_arg_idx.len())),
        )?;

        let max_len = std::cmp::max(acc_idx.len(), partial_arg_idx.len());
        let mut acc_idx_builder = Int32Builder::with_capacity(max_len);
        let mut partial_arg_idx_builder = Int32Builder::with_capacity(max_len);
        idx_for_zipped! {
            ((acc_idx, partial_arg_idx) in (acc_idx, partial_arg_idx)) => {
                acc_idx_builder.append_value(acc_idx as i32);
                partial_arg_idx_builder.append_value(partial_arg_idx as i32);
            }
        }
        let acc_idx = acc_idx_builder.finish();
        let partial_arg_idx = partial_arg_idx_builder.finish();

        partial_update_udaf(
            self.jcontext()?,
            params_batch,
            accs.obj.clone(),
            acc_idx,
            partial_arg_idx,
        )?;
        Ok(())
    }

    fn partial_merge(
        &self,
        accs: &mut AccColumnRef,
        acc_idx: IdxSelection<'_>,
        merging_accs: &mut AccColumnRef,
        merging_acc_idx: IdxSelection<'_>,
    ) -> Result<()> {
        let accs = downcast_any!(accs, mut AccUnsafeRowsColumn).unwrap();
        let merging_accs = downcast_any!(merging_accs, mut AccUnsafeRowsColumn).unwrap();

        let max_len = std::cmp::max(acc_idx.len(), merging_acc_idx.len());
        let mut acc_idx_builder = Int32Builder::with_capacity(max_len);
        let mut merging_acc_idx_builder = Int32Builder::with_capacity(max_len);
        idx_for_zipped! {
            ((acc_idx, merging_acc_idx) in (acc_idx, merging_acc_idx)) => {
                acc_idx_builder.append_value(acc_idx as i32);
                merging_acc_idx_builder.append_value(merging_acc_idx as i32);
            }
        }
        let acc_idx = acc_idx_builder.finish();
        let merging_acc_idx = merging_acc_idx_builder.finish();

        partial_merge_udaf(
            self.jcontext()?,
            accs.obj.clone(),
            merging_accs.obj.clone(),
            acc_idx,
            merging_acc_idx,
        )?;
        Ok(())
    }

    fn final_merge(&self, accs: &mut AccColumnRef, acc_idx: IdxSelection<'_>) -> Result<ArrayRef> {
        let accs = downcast_any!(accs, mut AccUnsafeRowsColumn).unwrap();
        final_merge_udaf(
            self.jcontext()?,
            accs.obj.clone(),
            acc_idx,
            self.import_schema.clone(),
        )
    }
}

struct AccUnsafeRowsColumn {
    obj: GlobalRef,
    jcontext: GlobalRef,
    num_rows: usize,
}

impl AccColumn for AccUnsafeRowsColumn {
    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn resize(&mut self, len: usize) {
        jni_call!(SparkUDAFWrapperContext(self.jcontext.as_obj()).resize(
            self.obj.as_obj(),
            len as i32,
        )-> ())
        .unwrap();
        self.num_rows = len;
    }

    fn shrink_to_fit(&mut self) {}

    fn num_records(&self) -> usize {
        self.num_rows
    }

    fn mem_used(&self) -> usize {
        jni_call!(
            SparkUDAFWrapperContext(self.jcontext.as_obj()).memUsed(
                self.obj.as_obj())
            -> i32).unwrap() as usize
    }

    fn freeze_to_rows(&self, idx: IdxSelection<'_>, array: &mut [Vec<u8>]) -> Result<()> {
        let idx_array: ArrayRef = Arc::new(idx.to_int32_array());
        let struct_array =
            StructArray::from(RecordBatch::try_new(index_schema(), vec![idx_array])?);
        let mut export_ffi_array = FFI_ArrowArray::new(&struct_array.to_data());
        let mut import_ffi_array = FFI_ArrowArray::empty();
        jni_call!(
            SparkUDAFWrapperContext(self.jcontext.as_obj()).serializeRows(
                self.obj.as_obj(),
                &mut export_ffi_array as *mut FFI_ArrowArray as i64,
                &mut import_ffi_array as *mut FFI_ArrowArray as i64,)
            -> ())?;
        // import output from context
        let import_ffi_schema = FFI_ArrowSchema::try_from(serialized_row_schema().as_ref())?;
        let import_struct_array =
            make_array(unsafe { from_ffi(import_ffi_array, &import_ffi_schema)? });
        let result_struct = import_struct_array.as_struct();

        let binary_array = downcast_any!(result_struct.column(0), BinaryArray)?;
        let data = binary_array.value(0);

        // UnsafeRow is serialized with big-endian i32 length prefix
        let mut cur = 0;
        for i in 0..array.len() {
            let bytes_len = i32::from_be_bytes(data[cur..][..4].try_into().unwrap()) as usize;
            write_len(bytes_len, &mut array[i])?;
            cur += 4;

            array[i].extend_from_slice(&data[cur..][..bytes_len]);
            cur += bytes_len;
        }
        Ok(())
    }

    fn unfreeze_from_rows(&mut self, array: &[&[u8]], offsets: &mut [usize]) -> Result<()> {
        let mut data = vec![];
        for (row_data, offset) in array.iter().zip(offsets) {
            let mut cur = Cursor::new(&row_data[*offset..]);
            let bytes_len = read_len(&mut cur)?;
            data.extend_from_slice(&(bytes_len as i32).to_be_bytes());
            *offset += cur.position() as usize;

            data.extend_from_slice(&row_data[*offset..][..bytes_len]);
            *offset += bytes_len;
        }

        let data_buffer = jni_new_direct_byte_buffer!(data)?;
        let rows = jni_call!(SparkUDAFWrapperContext(self.jcontext.as_obj())
            .deserializeRows(data_buffer.as_obj()) -> JObject)?;
        self.obj = jni_new_global_ref!(rows.as_obj())?;
        self.num_rows = array.len();
        Ok(())
    }

    fn spill(&self, idx: IdxSelection<'_>, buf: &mut SpillCompressedWriter) -> Result<()> {
        log::info!("start spill!");
        let idx_array: ArrayRef = Arc::new(idx.to_int32_array());
        let struct_array =
            StructArray::from(RecordBatch::try_new(index_schema(), vec![idx_array])?);
        let mut export_ffi_array = FFI_ArrowArray::new(&struct_array.to_data());
        let mut import_ffi_array = FFI_ArrowArray::empty();
        jni_call!(
            SparkUDAFWrapperContext(self.jcontext.as_obj()).serializeRows(
                self.obj.as_obj(),
                &mut export_ffi_array as *mut FFI_ArrowArray as i64,
                &mut import_ffi_array as *mut FFI_ArrowArray as i64,)
            -> ())?;
        // import output from context
        let import_ffi_schema = FFI_ArrowSchema::try_from(serialized_row_schema().as_ref())?;
        let import_struct_array =
            make_array(unsafe { from_ffi(import_ffi_array, &import_ffi_schema)? });
        let result_struct = import_struct_array.as_struct();

        let binary_array = downcast_any!(result_struct.column(0), BinaryArray)?;
        let data = binary_array.value(0);
        buf.write(data)?;
        log::info!("end spill!");
        Ok(())
    }

    fn unspill(&mut self, num_rows: usize, r: &mut SpillCompressedReader) -> Result<()> {
        log::info!("start unspill!");
        let mut data = vec![];
        let mut data_len = 0;
        for i in 0.. num_rows {
            let bytes_len = i32::from_be_bytes(data[data_len..][..4].try_into().unwrap()) as usize;
            data_len += bytes_len + 4;
        }
        let mut data = vec![];
        read_bytes_into_vec(r, &mut data, data_len)?;

        let data_buffer = jni_new_direct_byte_buffer!(data)?;
        let rows = jni_call!(SparkUDAFWrapperContext(self.jcontext.as_obj())
            .deserializeRows(data_buffer.as_obj()) -> JObject)?;
        self.obj = jni_new_global_ref!(rows.as_obj())?;
        self.num_rows = num_rows;

        log::info!("start unspill!");
        Ok(())
    }

}

fn int32_field() -> FieldRef {
    static FIELD: OnceCell<FieldRef> = OnceCell::new();
    FIELD
        .get_or_init(|| Arc::new(Field::new("", DataType::Int32, false)))
        .clone()
}

fn binary_field() -> FieldRef {
    static FIELD: OnceCell<FieldRef> = OnceCell::new();
    FIELD
        .get_or_init(|| Arc::new(Field::new("", DataType::Binary, false)))
        .clone()
}

fn index_schema() -> SchemaRef {
    static SCHEMA: OnceCell<SchemaRef> = OnceCell::new();
    SCHEMA
        .get_or_init(|| Arc::new(Schema::new(vec![int32_field()])))
        .clone()
}

fn index_tuple_schema() -> SchemaRef {
    static SCHEMA: OnceCell<SchemaRef> = OnceCell::new();
    SCHEMA
        .get_or_init(|| Arc::new(Schema::new(vec![int32_field(), int32_field()])))
        .clone()
}

fn serialized_row_schema() -> SchemaRef {
    static SCHEMA: OnceCell<SchemaRef> = OnceCell::new();
    SCHEMA
        .get_or_init(|| Arc::new(Schema::new(vec![binary_field()])))
        .clone()
}

fn partial_update_udaf(
    jcontext: GlobalRef,
    params_batch: RecordBatch,
    accs: GlobalRef,
    acc_idx: Int32Array,
    partial_arg_idx: Int32Array,
) -> Result<()> {
    let acc_idx: ArrayRef = Arc::new(acc_idx);
    let partial_arg_idx: ArrayRef = Arc::new(partial_arg_idx);
    let idx_struct_array = StructArray::from(RecordBatch::try_new(
        index_tuple_schema(),
        vec![acc_idx, partial_arg_idx],
    )?);
    let batch_struct_array = StructArray::from(params_batch);

    let mut export_ffi_idx_array = FFI_ArrowArray::new(&idx_struct_array.to_data());
    let mut export_ffi_batch_array = FFI_ArrowArray::new(&batch_struct_array.to_data());

    jni_call!(SparkUDAFWrapperContext(jcontext.as_obj()).update(
        accs.as_obj(),
        &mut export_ffi_idx_array as *mut FFI_ArrowArray as i64,
        &mut export_ffi_batch_array as *mut FFI_ArrowArray as i64,
    )-> ())?;

    Ok(())
}

fn partial_merge_udaf(
    jcontext: GlobalRef,
    accs: GlobalRef,
    merging_accs: GlobalRef,
    acc_idx: Int32Array,
    merging_acc_idx: Int32Array,
) -> Result<()> {
    let acc_idx: ArrayRef = Arc::new(acc_idx);
    let merging_acc_idx: ArrayRef = Arc::new(merging_acc_idx);
    let idx_struct_array = StructArray::from(RecordBatch::try_new(
        index_tuple_schema(),
        vec![acc_idx, merging_acc_idx],
    )?);
    let mut export_ffi_idx_array = FFI_ArrowArray::new(&idx_struct_array.to_data());

    jni_call!(SparkUDAFWrapperContext(jcontext.as_obj()).merge(
        accs.as_obj(),
        merging_accs.as_obj(),
        &mut export_ffi_idx_array as *mut FFI_ArrowArray as i64,
    )-> ())?;

    Ok(())
}

fn final_merge_udaf(
    jcontext: GlobalRef,
    accs: GlobalRef,
    acc_idx: IdxSelection<'_>,
    result_schema: SchemaRef,
) -> Result<ArrayRef> {
    let acc_idx: ArrayRef = Arc::new(Int32Array::from(acc_idx.to_int32_array()));
    let idx_struct_array = StructArray::from(RecordBatch::try_new(index_schema(), vec![acc_idx])?);
    let mut export_ffi_idx_array = FFI_ArrowArray::new(&idx_struct_array.to_data());
    let mut import_ffi_array = FFI_ArrowArray::empty();
    jni_call!(SparkUDAFWrapperContext(jcontext.as_obj()).eval(
        accs.as_obj(),
        &mut export_ffi_idx_array as *mut FFI_ArrowArray as i64,
        &mut import_ffi_array as *mut FFI_ArrowArray as i64,
    )-> ())?;

    // import output from context
    let import_ffi_schema = FFI_ArrowSchema::try_from(result_schema.as_ref())?;
    let import_struct_array =
        make_array(unsafe { from_ffi(import_ffi_array, &import_ffi_schema)? });
    let import_array = as_struct_array(&import_struct_array).column(0).clone();
    Ok(import_array)
}
