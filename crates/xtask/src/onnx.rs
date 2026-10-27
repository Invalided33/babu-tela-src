//! Just enough of ONNX's protobuf to store a model's weights in float16:
//! the fields of each message are read as they were written, those left
//! alone are copied byte for byte, and only the few that change are
//! written again. Each weight (a float tensor with at least `MIN_VALUES`
//! values) becomes a float16 tensor named `<name>__f16`, and a Cast node
//! at the head of the graph turns it back into the float32 `<name>` the
//! rest of the graph reads. The graph, its float32 arithmetic and the
//! model's metadata stay as they were.

use std::collections::HashSet;

use crate::{Result, fail};

/// Smaller tensors (a few biases, the constants) stay in float32: they
/// would save next to nothing.
pub const MIN_VALUES: u64 = 64;

/// What the float16 tensors and their Cast nodes are named after.
const F16: &str = "__f16";
const CAST: &str = "__cast";

// TensorProto.DataType and AttributeProto.AttributeType (onnx.proto).
const FLOAT: u64 = 1;
const FLOAT16: u64 = 10;
const ATTRIBUTE_INT: u64 = 2;

// Field numbers (onnx.proto).
const MODEL_GRAPH: u64 = 7;
const GRAPH_NODE: u64 = 1;
const GRAPH_INITIALIZER: u64 = 5;
const GRAPH_INPUT: u64 = 11;
const VALUE_INFO_NAME: u64 = 1;
const TENSOR_DIMS: u64 = 1;
const TENSOR_DATA_TYPE: u64 = 2;
const TENSOR_SEGMENT: u64 = 3;
const TENSOR_FLOAT_DATA: u64 = 4;
const TENSOR_NAME: u64 = 8;
const TENSOR_RAW_DATA: u64 = 9;
const TENSOR_EXTERNAL_DATA: u64 = 13;
const TENSOR_DATA_LOCATION: u64 = 14;
const NODE_INPUT: u64 = 1;
const NODE_OUTPUT: u64 = 2;
const NODE_NAME: u64 = 3;
const NODE_OP_TYPE: u64 = 4;
const NODE_ATTRIBUTE: u64 = 5;
const ATTRIBUTE_NAME: u64 = 1;
const ATTRIBUTE_I: u64 = 3;
const ATTRIBUTE_TYPE: u64 = 20;

// Wire types.
const VARINT: u8 = 0;
const FIXED64: u8 = 1;
const LEN: u8 = 2;
const FIXED32: u8 = 5;

/// A model with its weights in float16.
pub struct Converted {
    pub bytes: Vec<u8>,
    /// How many tensors went to float16.
    pub tensors: usize,
}

/// `model`, an ONNX file, with its weights in float16.
pub fn to_f16(model: &[u8]) -> Result<Converted> {
    let mut bytes = Vec::with_capacity(model.len() / 2);
    let (mut graphs, mut tensors) = (0, 0);
    for f in fields(model)? {
        if f.number == MODEL_GRAPH && f.wire == LEN {
            let (graph, n) = graph_to_f16(f.payload)?;
            put_bytes(&mut bytes, MODEL_GRAPH, &graph);
            graphs += 1;
            tensors += n;
        } else {
            bytes.extend_from_slice(f.raw);
        }
    }
    if graphs != 1 {
        fail!("o arquivo não parece um modelo ONNX ({graphs} grafos)");
    }
    if tensors == 0 {
        fail!("o modelo não tem pesos em float32 para converter");
    }
    Ok(Converted { bytes, tensors })
}

/// The graph with its weights in float16 and a Cast node for each at its
/// head: a node may only read what the nodes before it make.
fn graph_to_f16(graph: &[u8]) -> Result<(Vec<u8>, usize)> {
    let fields = fields(graph)?;
    let inputs = fields
        .iter()
        .filter(|f| f.number == GRAPH_INPUT && f.wire == LEN)
        .map(|f| value_info_name(f.payload))
        .collect::<Result<HashSet<String>>>()?;
    let mut casts = Vec::new();
    let mut rest = Vec::with_capacity(graph.len());
    let mut n = 0;
    for f in &fields {
        if f.number == GRAPH_INITIALIZER
            && f.wire == LEN
            && let Some((tensor, name)) = tensor_to_f16(f.payload)?
        {
            // An input of the same name may replace the weight when the
            // model runs; there would be two of it.
            if inputs.contains(&name) {
                fail!("o peso {name} também é uma entrada do modelo");
            }
            put_bytes(&mut casts, GRAPH_NODE, &cast(&name));
            put_bytes(&mut rest, GRAPH_INITIALIZER, &tensor);
            n += 1;
        } else {
            rest.extend_from_slice(f.raw);
        }
    }
    casts.extend_from_slice(&rest);
    Ok((casts, n))
}

fn value_info_name(info: &[u8]) -> Result<String> {
    for f in fields(info)? {
        if f.number == VALUE_INFO_NAME && f.wire == LEN {
            return Ok(std::str::from_utf8(f.payload)?.to_owned());
        }
    }
    Ok(String::new())
}

/// A weight in float16, named `<name>__f16`, with its name; None for any
/// other tensor, which stays as it is.
fn tensor_to_f16(tensor: &[u8]) -> Result<Option<(Vec<u8>, String)>> {
    let fields = fields(tensor)?;
    let (mut data_type, mut values) = (0, 1u64);
    let (mut name, mut data, mut elsewhere) = (None, None, false);
    for f in &fields {
        match (f.number, f.wire) {
            (TENSOR_DIMS, VARINT) => values = values.saturating_mul(f.value),
            (TENSOR_DIMS, LEN) => {
                let mut at = 0;
                while at < f.payload.len() {
                    values = values.saturating_mul(varint(f.payload, &mut at)?);
                }
            }
            (TENSOR_DATA_TYPE, VARINT) => data_type = f.value,
            (TENSOR_NAME, LEN) => name = Some(std::str::from_utf8(f.payload)?),
            (TENSOR_RAW_DATA | TENSOR_FLOAT_DATA, LEN) => data = Some(f.payload),
            (TENSOR_SEGMENT | TENSOR_EXTERNAL_DATA | TENSOR_DATA_LOCATION, _) => elsewhere = true,
            _ => {}
        }
    }
    if data_type != FLOAT || values < MIN_VALUES {
        return Ok(None);
    }
    let Some(name) = name else {
        fail!("o modelo tem um peso sem nome");
    };
    if elsewhere {
        fail!("o peso {name} não está dentro do arquivo");
    }
    // Both raw_data and a packed float_data hold little-endian floats.
    let Some(data) = data.filter(|d| values.checked_mul(4) == Some(d.len() as u64)) else {
        fail!("o peso {name} não traz os {values} valores que diz ter");
    };
    let half: Vec<u8> = data
        .as_chunks::<4>()
        .0
        .iter()
        .flat_map(|b| half::f16::from_f32(f32::from_le_bytes(*b)).to_le_bytes())
        .collect();
    // The same fields in the same order, with the new type, name and data.
    let mut out = Vec::with_capacity(tensor.len() / 2 + F16.len());
    for f in &fields {
        match (f.number, f.wire) {
            (TENSOR_DATA_TYPE, VARINT) => put_number(&mut out, TENSOR_DATA_TYPE, FLOAT16),
            (TENSOR_NAME, LEN) => {
                put_bytes(&mut out, TENSOR_NAME, format!("{name}{F16}").as_bytes())
            }
            (TENSOR_RAW_DATA | TENSOR_FLOAT_DATA, LEN) => {
                put_bytes(&mut out, TENSOR_RAW_DATA, &half)
            }
            _ => out.extend_from_slice(f.raw),
        }
    }
    Ok(Some((out, name.to_owned())))
}

/// The node that turns `<name>__f16` back into the float32 `<name>`.
fn cast(name: &str) -> Vec<u8> {
    let mut to = Vec::new();
    put_bytes(&mut to, ATTRIBUTE_NAME, b"to");
    put_number(&mut to, ATTRIBUTE_I, FLOAT);
    put_number(&mut to, ATTRIBUTE_TYPE, ATTRIBUTE_INT);
    let mut node = Vec::new();
    put_bytes(&mut node, NODE_INPUT, format!("{name}{F16}").as_bytes());
    put_bytes(&mut node, NODE_OUTPUT, name.as_bytes());
    put_bytes(&mut node, NODE_NAME, format!("{name}{CAST}").as_bytes());
    put_bytes(&mut node, NODE_OP_TYPE, b"Cast");
    put_bytes(&mut node, NODE_ATTRIBUTE, &to);
    node
}

/// One field of a message, as it was written.
struct Field<'a> {
    number: u64,
    wire: u8,
    /// A varint field's value.
    value: u64,
    /// A length-delimited field's payload.
    payload: &'a [u8],
    /// The whole field, key included.
    raw: &'a [u8],
}

fn fields(b: &[u8]) -> Result<Vec<Field<'_>>> {
    let mut out = Vec::new();
    let mut at = 0;
    while at < b.len() {
        let start = at;
        let key = varint(b, &mut at)?;
        let (number, wire) = (key >> 3, (key & 7) as u8);
        let (mut value, mut payload) = (0, &b[..0]);
        match wire {
            VARINT => value = varint(b, &mut at)?,
            FIXED64 => at += 8,
            FIXED32 => at += 4,
            LEN => {
                let n = usize::try_from(varint(b, &mut at)?)?;
                let end = at
                    .checked_add(n)
                    .filter(|&end| end <= b.len())
                    .ok_or("o modelo acaba no meio de um campo")?;
                payload = &b[at..end];
                at = end;
            }
            other => fail!("o modelo tem um campo de um tipo que não sei ler ({other})"),
        }
        if at > b.len() {
            fail!("o modelo acaba no meio de um campo");
        }
        out.push(Field {
            number,
            wire,
            value,
            payload,
            raw: &b[start..at],
        });
    }
    Ok(out)
}

fn varint(b: &[u8], at: &mut usize) -> Result<u64> {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let Some(&byte) = b.get(*at) else {
            fail!("o modelo acaba no meio de um número");
        };
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    fail!("o modelo tem um número de mais de 64 bits")
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_number(out: &mut Vec<u8>, number: u64, value: u64) {
    put_varint(out, (number << 3) | u64::from(VARINT));
    put_varint(out, value);
}

fn put_bytes(out: &mut Vec<u8>, number: u64, payload: &[u8]) {
    put_varint(out, (number << 3) | u64::from(LEN));
    put_varint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}
