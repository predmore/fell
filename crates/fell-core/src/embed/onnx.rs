//! Sentence embeddings from a local ONNX file, run with `tract-onnx`.
//!
//! tract is pure Rust (MIT OR Apache-2.0). It needs no ONNX Runtime library
//! and downloads nothing at build time, so the same build works on aarch64
//! and x86_64 Linux without a native artifact.
//!
//! The graph takes `input_ids`, `attention_mask` and `token_type_ids`
//! (`[batch, sequence]`, `i64`) and returns token states `[batch, sequence,
//! dims]`. One message is one sequence. The vector is the mean of the token
//! states, L2-normalised, which is what `all-MiniLM-L6-v2`'s pooling and
//! normalise modules do.

use std::path::Path;
use std::sync::Arc;

use tract_onnx::prelude::*;

use super::model_files::{LocalModel, ModelError, read_verified};
use super::wordpiece::WordPiece;
use super::{BodyEmbedder, EmbedderId};

type Plan = Arc<TypedRunnableModel>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Input {
    Ids,
    Mask,
    TokenTypes,
}

/// A loaded local model. Built off the UI thread by the indexer.
pub struct OnnxEmbedder {
    id: EmbedderId,
    plan: Plan,
    inputs: Vec<Input>,
    tokenizer: WordPiece,
    dims: usize,
    max_tokens: usize,
}

impl std::fmt::Debug for OnnxEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OnnxEmbedder")
            .field("id", &self.id)
            .field("dims", &self.dims)
            .field("max_tokens", &self.max_tokens)
            .finish_non_exhaustive()
    }
}

impl OnnxEmbedder {
    /// Read `model`'s files from `dir`, check them against their pinned
    /// SHA-256, and build the plan. The bytes that are hashed are the bytes
    /// that are loaded. This does not touch the network.
    pub fn load(model: &LocalModel, dir: &Path) -> Result<Self, ModelError> {
        let onnx = read_verified(dir, &model.onnx)?;
        let vocab = read_verified(dir, &model.vocab)?;
        let vocab = String::from_utf8(vocab)
            .map_err(|_| ModelError::Load("vocab.txt is not UTF-8".into()))?;
        Self::from_bytes(model.id, &onnx, &vocab, model.max_tokens)
    }

    fn from_bytes(
        id: EmbedderId,
        onnx: &[u8],
        vocab: &str,
        max_tokens: usize,
    ) -> Result<Self, ModelError> {
        let tokenizer = WordPiece::from_vocab(vocab)
            .ok_or_else(|| ModelError::Load("vocab.txt lacks [CLS], [SEP] or [UNK]".into()))?;
        let load = |err: TractError| ModelError::Load(format!("{err:#}"));
        let graph = tract_onnx::onnx()
            .model_for_read(&mut std::io::Cursor::new(onnx))
            .map_err(load)?;
        let mut inputs = Vec::new();
        for outlet in graph.input_outlets().map_err(load)? {
            inputs.push(match graph.node(outlet.node).name.as_str() {
                "input_ids" => Input::Ids,
                "attention_mask" => Input::Mask,
                "token_type_ids" => Input::TokenTypes,
                other => return Err(ModelError::Load(format!("unexpected input {other:?}"))),
            });
        }
        if !inputs.contains(&Input::Ids) {
            return Err(ModelError::Load("the graph has no input_ids".into()));
        }
        let plan = graph
            .into_optimized()
            .map_err(load)?
            .into_runnable()
            .map_err(load)?;
        let mut embedder = Self {
            id,
            plan,
            inputs,
            tokenizer,
            dims: 0,
            max_tokens: max_tokens.max(2),
        };
        // One run on `[CLS] [SEP]` checks the graph and learns its width.
        let probe = embedder
            .run(&embedder.tokenizer.encode("", 2))
            .map_err(load)?;
        if probe.is_empty() {
            return Err(ModelError::Load("the graph returned no dimensions".into()));
        }
        embedder.dims = probe.len();
        Ok(embedder)
    }

    /// Mean of the token states, before normalisation.
    fn run(&self, ids: &[i64]) -> TractResult<Vec<f32>> {
        let len = ids.len();
        let tensors = self
            .inputs
            .iter()
            .map(|input| {
                let values = match input {
                    Input::Ids => ids.to_vec(),
                    Input::Mask => vec![1i64; len],
                    Input::TokenTypes => vec![0i64; len],
                };
                Ok(Tensor::from(tract_ndarray::Array2::from_shape_vec((1, len), values)?).into())
            })
            .collect::<TractResult<TVec<TValue>>>()?;
        let outputs = self.plan.run(tensors)?;
        let states = outputs
            .first()
            .ok_or_else(|| TractError::msg("the graph has no output"))?
            .to_plain_array_view::<f32>()?;
        let shape = states.shape().to_vec();
        let (tokens, dims) = match shape[..] {
            [1, tokens, dims] if tokens == len && dims > 0 => (tokens, dims),
            _ => {
                return Err(TractError::msg(format!(
                    "expected [1, {len}, dims] token states, got {shape:?}"
                )));
            }
        };
        let mut mean = vec![0f32; dims];
        for token in 0..tokens {
            for (dim, value) in mean.iter_mut().enumerate() {
                *value += states[[0, token, dim]];
            }
        }
        let count = tokens as f32;
        for value in &mut mean {
            *value /= count;
        }
        Ok(mean)
    }
}

impl BodyEmbedder for OnnxEmbedder {
    fn id(&self) -> EmbedderId {
        self.id
    }

    fn dims(&self) -> usize {
        self.dims
    }

    /// A body with no word pieces is the zero vector, like the placeholder. A
    /// graph error is also the zero vector: the row is still written, so one
    /// bad message cannot make the indexer retry it forever.
    fn embed(&self, body: &str) -> Vec<f32> {
        let ids = self.tokenizer.encode(body, self.max_tokens);
        if ids.len() <= 2 {
            return vec![0.0; self.dims];
        }
        match self.run(&ids) {
            Ok(mut values) if values.len() == self.dims => {
                super::normalize(&mut values);
                values
            }
            _ => vec![0.0; self.dims],
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! A tiny ONNX graph with the same inputs and output as MiniLM. Each token
    //! id looks up a row of a small table (`Gather`), so the pooled vector is
    //! the mean of the rows. Tests build it in memory; nothing is downloaded.

    use prost::Message;
    use tract_onnx::pb::{
        AttributeProto, GraphProto, ModelProto, NodeProto, OperatorSetIdProto, TensorProto,
        TensorShapeProto, TypeProto, ValueInfoProto, attribute_proto::AttributeType,
        tensor_proto::DataType, tensor_shape_proto::Dimension, tensor_shape_proto::dimension,
        type_proto,
    };

    pub(crate) const TINY_VOCAB: &str =
        "[PAD]\n[UNK]\n[CLS]\n[SEP]\ninvoice\npay\nmarch\ncalendar\noffsite\nteam\n.\n";
    pub(crate) const TINY_DIMS: usize = 4;

    /// Row `id` of the lookup table.
    pub(crate) fn tiny_row(id: usize) -> [f32; TINY_DIMS] {
        let mut row = [0.0; TINY_DIMS];
        for (dim, value) in row.iter_mut().enumerate() {
            *value = ((id * 7 + dim * 3) % 11) as f32 - 5.0;
        }
        row
    }

    fn value_info(name: &str, elem: DataType, dims: &[&str]) -> ValueInfoProto {
        ValueInfoProto {
            name: name.into(),
            r#type: Some(TypeProto {
                denotation: String::new(),
                value: Some(type_proto::Value::TensorType(type_proto::Tensor {
                    elem_type: elem as i32,
                    shape: Some(TensorShapeProto {
                        dim: dims
                            .iter()
                            .map(|dim| Dimension {
                                denotation: String::new(),
                                value: Some(match dim.parse::<i64>() {
                                    Ok(n) => dimension::Value::DimValue(n),
                                    Err(_) => dimension::Value::DimParam((*dim).into()),
                                }),
                            })
                            .collect(),
                    }),
                })),
            }),
            doc_string: String::new(),
        }
    }

    pub(crate) fn tiny_onnx() -> Vec<u8> {
        let vocab_len = TINY_VOCAB.lines().count();
        let table: Vec<f32> = (0..vocab_len).flat_map(tiny_row).collect();
        let ids = ["batch_size", "sequence_length"];
        let graph = GraphProto {
            name: "tiny".into(),
            node: vec![NodeProto {
                name: "lookup".into(),
                op_type: "Gather".into(),
                input: vec!["table".into(), "input_ids".into()],
                output: vec!["last_hidden_state".into()],
                attribute: vec![AttributeProto {
                    name: "axis".into(),
                    r#type: AttributeType::Int as i32,
                    i: 0,
                    ..Default::default()
                }],
                ..Default::default()
            }],
            initializer: vec![TensorProto {
                name: "table".into(),
                dims: vec![vocab_len as i64, TINY_DIMS as i64],
                data_type: DataType::Float as i32,
                float_data: table,
                ..Default::default()
            }],
            input: vec![
                value_info("input_ids", DataType::Int64, &ids),
                value_info("attention_mask", DataType::Int64, &ids),
                value_info("token_type_ids", DataType::Int64, &ids),
            ],
            output: vec![value_info(
                "last_hidden_state",
                DataType::Float,
                &["batch_size", "sequence_length", "4"],
            )],
            ..Default::default()
        };
        ModelProto {
            ir_version: 8,
            opset_import: vec![OperatorSetIdProto {
                domain: String::new(),
                version: 14,
            }],
            producer_name: "fell-test".into(),
            graph: Some(graph),
            ..Default::default()
        }
        .encode_to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::embed::model_files::MINILM_L6_V2_INT8;

    const ID: EmbedderId = EmbedderId {
        model: "tiny-onnx",
        version: "1",
    };

    fn tiny(max_tokens: usize) -> OnnxEmbedder {
        OnnxEmbedder::from_bytes(ID, &tiny_onnx(), TINY_VOCAB, max_tokens).unwrap()
    }

    fn expected(ids: &[usize]) -> Vec<f32> {
        let mut mean = vec![0.0f32; TINY_DIMS];
        for id in ids {
            for (value, row) in mean.iter_mut().zip(tiny_row(*id)) {
                *value += row;
            }
        }
        for value in &mut mean {
            *value /= ids.len() as f32;
        }
        crate::embed::normalize(&mut mean);
        mean
    }

    fn close(left: &[f32], right: &[f32]) -> bool {
        left.len() == right.len() && left.iter().zip(right).all(|(a, b)| (a - b).abs() < 1e-5)
    }

    #[test]
    fn mean_pools_the_token_states_and_normalises() {
        let embedder = tiny(256);
        assert_eq!(embedder.id(), ID);
        assert_eq!(embedder.dims(), TINY_DIMS);
        // [CLS] pay march invoice . [SEP]
        let got = embedder.embed("Pay MARCH invoice.");
        assert!(close(&got, &expected(&[2, 5, 6, 4, 10, 3])), "{got:?}");
        let norm: f32 = got.iter().map(|v| v * v).sum();
        assert!((norm.sqrt() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn truncates_at_max_tokens() {
        let embedder = tiny(4);
        // [CLS] team offsite [SEP]; "calendar" is past the limit.
        let got = embedder.embed("team offsite calendar");
        assert!(close(&got, &expected(&[2, 9, 8, 3])), "{got:?}");
    }

    #[test]
    fn an_empty_body_is_the_zero_vector() {
        let embedder = tiny(256);
        assert_eq!(embedder.embed(" \n\t"), vec![0.0; TINY_DIMS]);
        assert_eq!(embedder.embed(""), vec![0.0; TINY_DIMS]);
    }

    #[test]
    fn a_graph_that_is_not_onnx_does_not_load() {
        let err = OnnxEmbedder::from_bytes(ID, b"not a model", TINY_VOCAB, 8).unwrap_err();
        assert!(matches!(err, ModelError::Load(_)), "{err}");
        let err = OnnxEmbedder::from_bytes(ID, &tiny_onnx(), "no markers", 8).unwrap_err();
        assert!(matches!(err, ModelError::Load(_)), "{err}");
    }

    /// Manual check against the real pinned weights. Not run in CI and never
    /// downloads: point `FELL_TEST_MODEL_DIR` at a directory that holds the
    /// verified `model.onnx` and `vocab.txt`, then run
    /// `cargo test -p fell-core --release real_minilm -- --ignored --nocapture`.
    #[test]
    #[ignore = "needs the downloaded model; set FELL_TEST_MODEL_DIR"]
    fn real_minilm_ranks_related_mail_higher() {
        let Some(dir) = std::env::var_os("FELL_TEST_MODEL_DIR") else {
            panic!("set FELL_TEST_MODEL_DIR to the model directory");
        };
        let model = MINILM_L6_V2_INT8;
        let start = std::time::Instant::now();
        let embedder = OnnxEmbedder::load(&model, Path::new(&dir)).unwrap();
        eprintln!("load and verify: {:?}", start.elapsed());
        assert_eq!(embedder.dims(), 384);
        let cos = |a: &[f32], b: &[f32]| a.iter().zip(b).map(|(x, y)| x * y).sum::<f32>();
        let invoice = embedder.embed("Please pay the March invoice by Friday.");
        let due = embedder.embed("The invoice for March is due at the end of the week.");
        let offsite = embedder.embed("Team offsite: calendar invite for next Tuesday.");
        assert!(cos(&invoice, &due) > 0.6, "{}", cos(&invoice, &due));
        assert!(cos(&invoice, &due) > cos(&invoice, &offsite) + 0.3);
        let bodies = [
            "Thanks, see you then.".to_string(),
            "Hi all, following up on yesterday's planning meeting. ".repeat(6),
            "Hello team, following up on the quarterly planning meeting. ".repeat(40),
        ];
        for body in &bodies {
            let tokens = embedder.tokenizer.encode(body, model.max_tokens).len();
            let runs = 20;
            let start = std::time::Instant::now();
            for _ in 0..runs {
                std::hint::black_box(embedder.embed(body));
            }
            eprintln!(
                "{tokens:>3} tokens: {:?} per message",
                start.elapsed() / runs
            );
        }
    }
}
