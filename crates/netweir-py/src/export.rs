//! Item exporters that serialise in Rust: JSON Lines, CSV and Parquet.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufWriter, Write};
use std::sync::{Arc, Mutex};

use parquet::basic::{Compression, LogicalType, Repetition, Type as Physical};
use parquet::data_type::{BoolType, ByteArray, ByteArrayType, DoubleType, Int64Type};
use parquet::file::properties::WriterProperties;
use parquet::file::writer::SerializedFileWriter;
use parquet::schema::types::Type;

use pyo3::exceptions::{PyTypeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBool, PyDict, PyFloat, PyInt, PyList, PyString, PyTuple};
use serde_json::{Map, Value};

/// An item (dict, dataclass, list or scalar) as JSON. Dates and times
/// become ISO 8601 strings; floats that JSON can't hold (NaN, infinities)
/// become null.
fn to_json(obj: &Bound<'_, PyAny>, depth: usize) -> PyResult<Value> {
    if depth > 100 {
        return Err(PyValueError::new_err(
            "item nests deeper than 100 levels (a cycle?)",
        ));
    }
    if obj.is_none() {
        return Ok(Value::Null);
    }
    if let Ok(b) = obj.cast::<PyBool>() {
        return Ok(Value::Bool(b.is_true()));
    }
    if obj.is_instance_of::<PyInt>() {
        if let Ok(i) = obj.extract::<i64>() {
            return Ok(Value::from(i));
        }
        if let Ok(u) = obj.extract::<u64>() {
            return Ok(Value::from(u));
        }
        // Too big for 64 bits: keep every digit, as a string.
        return Ok(Value::String(obj.str()?.to_string()));
    }
    if let Ok(f) = obj.cast::<PyFloat>() {
        return Ok(serde_json::Number::from_f64(f.value()).map_or(Value::Null, Value::Number));
    }
    if let Ok(s) = obj.cast::<PyString>() {
        return Ok(Value::String(s.to_string()));
    }
    if let Ok(d) = obj.cast::<PyDict>() {
        let mut map = Map::new();
        for (k, v) in d.iter() {
            let key = match k.cast::<PyString>() {
                Ok(s) => s.to_string(),
                Err(_) => k.str()?.to_string(),
            };
            map.insert(key, to_json(&v, depth + 1)?);
        }
        return Ok(Value::Object(map));
    }
    if obj.is_instance_of::<PyList>() || obj.is_instance_of::<PyTuple>() {
        let mut out = Vec::new();
        for v in obj.try_iter()? {
            out.push(to_json(&v?, depth + 1)?);
        }
        return Ok(Value::Array(out));
    }
    let py = obj.py();
    if obj.hasattr("__dataclass_fields__")? {
        let as_dict = py.import("dataclasses")?.getattr("asdict")?.call1((obj,))?;
        return to_json(&as_dict, depth + 1);
    }
    if obj.hasattr("isoformat")? {
        return Ok(Value::String(obj.call_method0("isoformat")?.extract()?));
    }
    if obj.is_instance(&py.import("decimal")?.getattr("Decimal")?)? {
        return Ok(Value::String(obj.str()?.to_string()));
    }
    Err(PyTypeError::new_err(format!(
        "can't export a {} (use dicts, lists, str, numbers, bool, None, dates or dataclasses)",
        obj.get_type().name()?
    )))
}

fn open(path: &str) -> PyResult<BufWriter<File>> {
    open_for(path, false)
}

/// The file, emptied, or with `append` kept and added to.
fn open_for(path: &str, append: bool) -> PyResult<BufWriter<File>> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true);
    if append {
        options.append(true);
    } else {
        options.write(true).truncate(true);
    }
    options
        .open(path)
        .map(BufWriter::new)
        .map_err(|e| PyValueError::new_err(format!("can't write {path}: {e}")))
}

fn io_error(e: std::io::Error) -> PyErr {
    pyo3::exceptions::PyOSError::new_err(e.to_string())
}

/// Writes one JSON object per line.
#[pyclass(frozen, module = "netweir")]
pub struct JsonlWriter {
    out: Mutex<Option<BufWriter<File>>>,
}

#[pymethods]
impl JsonlWriter {
    /// With `append`, items go after what the file already holds.
    #[new]
    #[pyo3(signature = (path, append=false))]
    fn new(path: &str, append: bool) -> PyResult<JsonlWriter> {
        Ok(JsonlWriter {
            out: Mutex::new(Some(open_for(path, append)?)),
        })
    }

    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let value = to_json(item, 0)?;
        let mut guard = self.out.lock().unwrap_or_else(|e| e.into_inner());
        let out = guard
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("the writer is closed"))?;
        serde_json::to_writer(&mut *out, &value).map_err(|e| io_error(e.into()))?;
        out.write_all(b"\n").map_err(io_error)
    }

    /// Pushes what's buffered to the file and asks the OS to keep it.
    fn flush(&self) -> PyResult<()> {
        if let Some(out) = self.out.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            out.flush().map_err(io_error)?;
            out.get_ref().sync_data().map_err(io_error)?;
        }
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        if let Some(mut out) = self.out.lock().unwrap_or_else(|e| e.into_inner()).take() {
            out.flush().map_err(io_error)?;
        }
        Ok(())
    }
}

/// Writes items as CSV rows under a header line.
#[pyclass(frozen, module = "netweir")]
pub struct CsvWriter {
    state: Mutex<CsvState>,
}

struct CsvState {
    out: Option<csv::Writer<BufWriter<File>>>,
    /// Column names; fixed by `fields` or by the first item's keys.
    fields: Option<Vec<String>>,
    header_written: bool,
}

fn cell(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        nested => nested.to_string(),
    }
}

#[pymethods]
impl CsvWriter {
    /// With `append`, rows go after what the file already holds, and a
    /// file that isn't empty is taken to have its header.
    #[new]
    #[pyo3(signature = (path, fields=None, append=false))]
    fn new(path: &str, fields: Option<Vec<String>>, append: bool) -> PyResult<CsvWriter> {
        let has_header = append && std::fs::metadata(path).is_ok_and(|m| m.len() > 0);
        let out = csv::Writer::from_writer(open_for(path, append)?);
        Ok(CsvWriter {
            state: Mutex::new(CsvState {
                out: Some(out),
                fields,
                header_written: has_header,
            }),
        })
    }

    /// Writes the item's row. Returns the keys it had that aren't columns,
    /// which are left out of the file.
    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<Vec<String>> {
        let Value::Object(map) = to_json(item, 0)? else {
            return Err(PyTypeError::new_err(
                "CSV rows need dict (or dataclass) items",
            ));
        };
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        let out = state
            .out
            .as_mut()
            .ok_or_else(|| PyValueError::new_err("the writer is closed"))?;
        let csv_err = |e: csv::Error| io_error(std::io::Error::other(e));
        let fields = state
            .fields
            .get_or_insert_with(|| map.keys().cloned().collect());
        if !state.header_written {
            out.write_record(fields.iter()).map_err(csv_err)?;
            state.header_written = true;
        }
        out.write_record(
            fields
                .iter()
                .map(|f| map.get(f).map(cell).unwrap_or_default()),
        )
        .map_err(csv_err)?;
        Ok(map
            .keys()
            .filter(|k| !fields.contains(k))
            .cloned()
            .collect())
    }

    /// Pushes what's buffered to the file and asks the OS to keep it.
    fn flush(&self) -> PyResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(out) = guard.out.as_mut() {
            out.flush().map_err(io_error)?;
            out.get_ref().get_ref().sync_data().map_err(io_error)?;
        }
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        let mut guard = self.state.lock().unwrap_or_else(|e| e.into_inner());
        let state = &mut *guard;
        if let Some(mut out) = state.out.take() {
            // No items arrived: still write the header if columns were given.
            if let (false, Some(fields)) = (state.header_written, &state.fields) {
                out.write_record(fields.iter())
                    .map_err(|e| io_error(std::io::Error::other(e)))?;
            }
            out.flush().map_err(io_error)?;
        }
        Ok(())
    }
}

/// Items read before the Parquet schema is fixed.
const SAMPLE: usize = 1_000;
/// Rows per Parquet row group.
const ROW_GROUP: usize = 10_000;

/// What a Parquet column holds.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bool,
    Int,
    Double,
    Text,
}

impl Kind {
    /// The kind a value asks for; `None` for null, which fits anything.
    fn of(value: &Value) -> Option<Kind> {
        match value {
            Value::Null => None,
            Value::Bool(_) => Some(Kind::Bool),
            Value::Number(n) if n.is_i64() => Some(Kind::Int),
            // Above i64::MAX: no Parquet integer holds it exactly.
            Value::Number(n) if n.is_u64() => Some(Kind::Text),
            Value::Number(_) => Some(Kind::Double),
            _ => Some(Kind::Text),
        }
    }

    /// The narrowest kind holding both: integers and doubles make a
    /// double, any other mix makes text.
    fn merge(self, other: Kind) -> Kind {
        match (self, other) {
            (a, b) if a == b => a,
            (Kind::Int, Kind::Double) | (Kind::Double, Kind::Int) => Kind::Double,
            _ => Kind::Text,
        }
    }
}

/// One column's values for the row group being built, with a definition
/// level per row (0 for null).
enum Values {
    Bool(Vec<bool>),
    Int(Vec<i64>),
    Double(Vec<f64>),
    Text(Vec<ByteArray>),
}

struct Column {
    name: String,
    values: Values,
    defined: Vec<i16>,
}

impl Column {
    fn new(name: String, kind: Kind) -> Column {
        let values = match kind {
            Kind::Bool => Values::Bool(Vec::new()),
            Kind::Int => Values::Int(Vec::new()),
            Kind::Double => Values::Double(Vec::new()),
            Kind::Text => Values::Text(Vec::new()),
        };
        Column {
            name,
            values,
            defined: Vec::new(),
        }
    }

    fn schema(&self) -> PyResult<Arc<Type>> {
        let (physical, logical) = match self.values {
            Values::Bool(_) => (Physical::BOOLEAN, None),
            Values::Int(_) => (Physical::INT64, None),
            Values::Double(_) => (Physical::DOUBLE, None),
            Values::Text(_) => (Physical::BYTE_ARRAY, Some(LogicalType::String)),
        };
        Type::primitive_type_builder(&self.name, physical)
            .with_repetition(Repetition::OPTIONAL)
            .with_logical_type(logical)
            .build()
            .map(Arc::new)
            .map_err(parquet_error)
    }

    /// Adds the row's value. False if it doesn't fit the column, in which
    /// case the row gets a null.
    fn push(&mut self, value: Option<&Value>) -> bool {
        let fits = match (&mut self.values, value) {
            (_, None | Some(Value::Null)) => {
                self.defined.push(0);
                return true;
            }
            (Values::Bool(v), Some(Value::Bool(b))) => {
                v.push(*b);
                true
            }
            (Values::Int(v), Some(Value::Number(n))) => n.as_i64().map(|i| v.push(i)).is_some(),
            (Values::Double(v), Some(Value::Number(n))) => n.as_f64().map(|f| v.push(f)).is_some(),
            (Values::Text(v), Some(other)) => {
                v.push(ByteArray::from(cell(other).into_bytes()));
                true
            }
            _ => false,
        };
        self.defined.push(i16::from(fits));
        fits
    }

    fn clear(&mut self) {
        self.defined.clear();
        match &mut self.values {
            Values::Bool(v) => v.clear(),
            Values::Int(v) => v.clear(),
            Values::Double(v) => v.clear(),
            Values::Text(v) => v.clear(),
        }
    }
}

fn parquet_error(e: parquet::errors::ParquetError) -> PyErr {
    pyo3::exceptions::PyOSError::new_err(format!("parquet: {e}"))
}

/// Values a Parquet file couldn't hold, by key: (no column, wrong type).
type Problems = BTreeMap<String, (u64, u64)>;

struct ParquetState {
    file: Option<BufWriter<File>>,
    sample: Vec<Map<String, Value>>,
    columns: Vec<Column>,
    rows: usize,
    out: Option<SerializedFileWriter<BufWriter<File>>>,
    problems: Problems,
    closed: bool,
}

impl ParquetState {
    /// Fixes the schema from the sampled items, then writes them.
    fn start(&mut self) -> PyResult<()> {
        let mut kinds: Vec<(String, Option<Kind>)> = Vec::new();
        for item in &self.sample {
            for (key, value) in item {
                let kind = Kind::of(value);
                match kinds.iter_mut().find(|(k, _)| k == key) {
                    Some((_, seen)) => {
                        *seen = match (*seen, kind) {
                            (Some(a), Some(b)) => Some(a.merge(b)),
                            (a, b) => a.or(b),
                        }
                    }
                    None => kinds.push((key.clone(), kind)),
                }
            }
        }
        self.columns = kinds
            .into_iter()
            .map(|(name, kind)| Column::new(name, kind.unwrap_or(Kind::Text)))
            .collect();
        let fields = self
            .columns
            .iter()
            .map(Column::schema)
            .collect::<PyResult<Vec<_>>>()?;
        let schema = Type::group_type_builder("item")
            .with_fields(fields)
            .build()
            .map_err(parquet_error)?;
        let props = WriterProperties::builder()
            .set_compression(Compression::SNAPPY)
            .build();
        let file = self.file.take().expect("parquet writer started twice");
        self.out = Some(
            SerializedFileWriter::new(file, Arc::new(schema), Arc::new(props))
                .map_err(parquet_error)?,
        );
        for item in std::mem::take(&mut self.sample) {
            self.add(&item)?;
        }
        Ok(())
    }

    fn add(&mut self, item: &Map<String, Value>) -> PyResult<()> {
        for column in &mut self.columns {
            if !column.push(item.get(&column.name)) {
                self.problems.entry(column.name.clone()).or_default().1 += 1;
            }
        }
        for key in item.keys() {
            if !self.columns.iter().any(|c| &c.name == key) {
                self.problems.entry(key.clone()).or_default().0 += 1;
            }
        }
        self.rows += 1;
        if self.rows == ROW_GROUP {
            self.flush()?;
        }
        Ok(())
    }

    /// Writes the buffered rows as one row group.
    fn flush(&mut self) -> PyResult<()> {
        if self.rows == 0 {
            return Ok(());
        }
        let out = self.out.as_mut().expect("parquet rows before the schema");
        let mut group = out.next_row_group().map_err(parquet_error)?;
        for column in &mut self.columns {
            let mut writer = group
                .next_column()
                .map_err(parquet_error)?
                .expect("one column writer per schema field");
            let defined = Some(&column.defined[..]);
            match &column.values {
                Values::Bool(v) => writer.typed::<BoolType>().write_batch(v, defined, None),
                Values::Int(v) => writer.typed::<Int64Type>().write_batch(v, defined, None),
                Values::Double(v) => writer.typed::<DoubleType>().write_batch(v, defined, None),
                Values::Text(v) => writer
                    .typed::<ByteArrayType>()
                    .write_batch(v, defined, None),
            }
            .map_err(parquet_error)?;
            writer.close().map_err(parquet_error)?;
            column.clear();
        }
        group.close().map_err(parquet_error)?;
        self.rows = 0;
        Ok(())
    }
}

/// Writes items to a Parquet file. Column names and types come from the
/// first 1,000 items; see `write` for what happens to values that don't
/// fit them.
#[pyclass(frozen, module = "netweir")]
pub struct ParquetWriter {
    state: Mutex<ParquetState>,
}

#[pymethods]
impl ParquetWriter {
    #[new]
    fn new(path: &str) -> PyResult<ParquetWriter> {
        Ok(ParquetWriter {
            state: Mutex::new(ParquetState {
                file: Some(open(path)?),
                sample: Vec::new(),
                columns: Vec::new(),
                rows: 0,
                out: None,
                problems: Problems::new(),
                closed: false,
            }),
        })
    }

    /// Adds the item as a row. Once the schema is fixed, a key with no
    /// column is left out, an integer in a double column is widened, any
    /// value in a text column is stored as text, and any other mismatch is
    /// stored as null. `close` reports what was left out.
    fn write(&self, item: &Bound<'_, PyAny>) -> PyResult<()> {
        let Value::Object(map) = to_json(item, 0)? else {
            return Err(PyTypeError::new_err(
                "Parquet rows need dict (or dataclass) items",
            ));
        };
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Err(PyValueError::new_err("the writer is closed"));
        }
        if state.out.is_none() {
            state.sample.push(map);
            if state.sample.len() == SAMPLE {
                state.start()?;
            }
            return Ok(());
        }
        state.add(&map)
    }

    /// Finishes the file. Returns, for each key some values of which
    /// couldn't be stored, how many had no column and how many had the
    /// wrong type.
    fn close(&self) -> PyResult<Vec<(String, u64, u64)>> {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.closed {
            return Ok(Vec::new());
        }
        state.closed = true;
        if state.out.is_none() {
            state.start()?;
        }
        state.flush()?;
        if let Some(out) = state.out.take() {
            let mut file = out.into_inner().map_err(parquet_error)?;
            file.flush().map_err(io_error)?;
        }
        Ok(std::mem::take(&mut state.problems)
            .into_iter()
            .map(|(k, (missing, wrong))| (k, missing, wrong))
            .collect())
    }
}

pub fn register(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<JsonlWriter>()?;
    m.add_class::<CsvWriter>()?;
    m.add_class::<ParquetWriter>()?;
    Ok(())
}
