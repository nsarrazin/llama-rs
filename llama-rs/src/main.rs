#![feature(buf_read_has_data_left)]

use std::{
    collections::HashMap,
    io::{self, BufRead, Read, Seek, SeekFrom, Write},
    path::Path,
};

use anyhow::{Context, Result};

use ggml::{GgmlContext, GgmlTensor};
use ggml_raw::{ggml_context, ggml_init_params, ggml_tensor, ggml_type};
use regex::Regex;

use crate::ggml::{GGML_TYPE_F16, GGML_TYPE_F32, GGML_TYPE_Q4_0, GGML_TYPE_Q4_1};

mod ggml;

#[derive(Debug, Default)]
pub struct LlamaHyperParams {
    n_vocab: i32,
    n_ctx: i32,
    n_embd: i32,
    n_mult: i32,
    n_head: i32,
    n_layer: i32,
    n_rot: i32,
    f16_: i32,
}

struct LlamaLayer {
    attention_norm: GgmlTensor,

    wq: GgmlTensor,
    wk: GgmlTensor,
    wv: GgmlTensor,
    wo: GgmlTensor,

    // normalization
    ffn_norm: GgmlTensor,

    // ff
    w1: GgmlTensor,
    w2: GgmlTensor,
    w3: GgmlTensor,
}

struct LlamaModel {
    hparams: LlamaHyperParams,

    tok_embeddings: GgmlTensor,

    norm: GgmlTensor,
    output: GgmlTensor,

    layers: Vec<LlamaLayer>,

    memory_k: GgmlTensor,
    memory_v: GgmlTensor,

    tensors: HashMap<String, GgmlTensor>,

    context: GgmlContext,
}

type TokenId = i32;
type Token = String;

#[derive(Default)]
struct GptVocab {
    /// Maps every integer (index) token id to its corresponding string
    mapping: Vec<String>,
}

fn llama_n_parts(size: i32) -> i32 {
    match size {
        4096 => 1,
        5120 => 2,
        6656 => 3,
        8192 => 8,
        _ => unreachable!("Invalid size for N_PARTS"),
    }
}

impl LlamaModel {
    fn load(path: impl AsRef<Path>, n_ctx: i32) -> Result<(LlamaModel, GptVocab)> {
        use std::fs::File;
        use std::io::BufReader;

        let path = path.as_ref();
        let path_str = path.to_string_lossy();

        let mut reader = BufReader::new(
            File::open(&path)
                .with_context(|| anyhow::anyhow!("Failed to open file at '{path_str}'",))?,
        );

        /// Helper function. Reads an int from the buffer and returns it.
        fn read_int(reader: &mut impl BufRead) -> Result<i32> {
            let mut bytes = [0u8; 4];
            reader
                .read_exact(&mut bytes)
                .context("Trying to parse metadata")?;
            Ok(i32::from_le_bytes(bytes))
        }

        /// Helper function. Reads a string from the buffer and returns it.
        fn read_string(reader: &mut BufReader<File>, len: usize) -> Result<String> {
            let mut buf = vec![0; len];
            reader.read_exact(&mut buf)?;
            let s = String::from_utf8(buf)?;
            Ok(s)
        }

        // Verify magic
        {
            let mut magic = read_int(&mut reader)?;
            if magic != 0x67676d6c {
                anyhow::bail!("Invalid model file '{path_str}' (bad magic)")
            }
        }

        // =================
        // Load hyper params
        // =================

        // NOTE: Field order matters! Data is laid out in the file exactly
        // in this order.
        let hparams = LlamaHyperParams {
            n_vocab: read_int(&mut reader)?,
            n_ctx,
            n_embd: read_int(&mut reader)?,
            n_mult: read_int(&mut reader)?,
            n_head: read_int(&mut reader)?,
            n_layer: read_int(&mut reader)?,
            n_rot: read_int(&mut reader)?,
            f16_: read_int(&mut reader)?,
        };

        let n_ff =
            ((2 * (4 * hparams.n_embd) / 3 + hparams.n_mult - 1) / hparams.n_mult) * hparams.n_mult;
        let n_parts = llama_n_parts(hparams.n_embd);

        eprintln!("Loaded HyperParams {hparams:#?}");

        // ===============
        // Load vocabulary
        // ===============
        let mut vocab = GptVocab::default();
        for _ in 0..hparams.n_vocab {
            let len = read_int(&mut reader)?;
            let word = read_string(&mut reader, len as usize)?;
            vocab.mapping.push(word);
        }

        // for the big tensors, we have the option to store the data in 16-bit
        // floats or quantized in order to save memory and also to speed up the
        // computation
        let wtype = match hparams.f16_ {
            0 => GGML_TYPE_F32,
            1 => GGML_TYPE_F16,
            2 => GGML_TYPE_Q4_0,
            3 => GGML_TYPE_Q4_1,
            invalid => anyhow::bail!("Invalid value for hparams.f16_ {invalid}"),
        };

        let wtype2 = ggml_raw::ggml_type_GGML_TYPE_F32;

        let n_embd = hparams.n_embd;
        let n_layer = hparams.n_layer;
        let n_ctx = hparams.n_ctx;
        let n_vocab = hparams.n_vocab;

        let ctx_size = {
            // Use 64-bit math to prevent overflow.
            let n_embd = n_embd as u64;
            let n_layer = n_layer as u64;
            let n_ctx = n_ctx as u64;
            let n_vocab = n_vocab as u64;
            let n_ff = n_ff as u64;

            macro_rules! mul {
                ($term:expr, $($terms:expr),*) => {
                    (($term as f64) $(* ($terms as f64))*) as u64
                };
            }

            fn ggml_type_sizef(x: u32) -> f64 {
                (unsafe { ggml_raw::ggml_type_sizef(x) }) as f64
            }

            let mut ctx_size: u64 = 0;

            ctx_size += mul!(n_embd, n_vocab, ggml_type_sizef(wtype)); // tok_embeddings

            ctx_size += mul!(n_embd, ggml_type_sizef(GGML_TYPE_F32)); // norm

            ctx_size += mul!(n_embd, n_vocab, ggml_type_sizef(wtype)); // output

            ctx_size += mul!(n_layer, n_embd, ggml_type_sizef(GGML_TYPE_F32)); // attention_norm

            ctx_size += mul!(n_layer, n_embd, n_embd, ggml_type_sizef(wtype)); // wq
            ctx_size += mul!(n_layer, n_embd, n_embd, ggml_type_sizef(wtype)); // wk
            ctx_size += mul!(n_layer, n_embd, n_embd, ggml_type_sizef(wtype)); // wv
            ctx_size += mul!(n_layer, n_embd, n_embd, ggml_type_sizef(wtype)); // wo

            ctx_size += mul!(n_layer, n_embd, ggml_type_sizef(GGML_TYPE_F32)); // ffn_norm

            ctx_size += mul!(n_layer, n_ff, n_embd, ggml_type_sizef(wtype)); // w1
            ctx_size += mul!(n_layer, n_ff, n_embd, ggml_type_sizef(wtype)); // w2
            ctx_size += mul!(n_layer, n_ff, n_embd, ggml_type_sizef(wtype)); // w3

            ctx_size += mul!(n_ctx, n_layer, n_embd, ggml_type_sizef(GGML_TYPE_F32)); // memory_k
            ctx_size += mul!(n_ctx, n_layer, n_embd, ggml_type_sizef(GGML_TYPE_F32)); // memory_v

            ctx_size += (5 + 10 * n_layer) * 256; // object overhead

            // TODO: Sizes in the original implementation are slightly smaller
            // than the ones we get here, but everything still works. I don't
            // know where the issue is.
            println!(
                "ggml ctx size = {:.2} MB\n",
                ctx_size as f64 / (1024.0 * 1024.0)
            );

            ctx_size
        };

        // Initialize the context
        let context = GgmlContext::init(ggml_init_params {
            mem_size: ctx_size as usize,
            mem_buffer: std::ptr::null_mut(),
        });

        let model = {
            let mut tensors = HashMap::new();

            let tok_embeddings = context.new_tensor_2d(wtype, n_embd, n_vocab);
            let norm = context.new_tensor_1d(GGML_TYPE_F32, n_embd);
            let output = context.new_tensor_2d(wtype, n_embd, n_vocab);

            tensors.insert("tok_embeddings.weight".to_owned(), tok_embeddings.share());
            tensors.insert("norm.weight".to_owned(), norm.share());
            tensors.insert("output.weight".to_owned(), output.share());

            let mut layers = Vec::new();
            for i in 0..n_layer {
                let layer = LlamaLayer {
                    attention_norm: context.new_tensor_1d(GGML_TYPE_F32, n_embd),
                    wq: context.new_tensor_2d(wtype, n_embd, n_embd),
                    wk: context.new_tensor_2d(wtype, n_embd, n_embd),
                    wv: context.new_tensor_2d(wtype, n_embd, n_embd),
                    wo: context.new_tensor_2d(wtype, n_embd, n_embd),
                    ffn_norm: context.new_tensor_1d(GGML_TYPE_F32, n_embd),
                    w1: context.new_tensor_2d(wtype, n_embd, n_ff),
                    w2: context.new_tensor_2d(wtype, n_ff, n_embd),
                    w3: context.new_tensor_2d(wtype, n_embd, n_ff),
                };

                tensors.insert(
                    format!("layers.{i}.attention_norm.weight"),
                    layer.attention_norm.share(),
                );

                tensors.insert(format!("layers.{i}.attention.wq.weight"), layer.wq.share());
                tensors.insert(format!("layers.{i}.attention.wk.weight"), layer.wk.share());
                tensors.insert(format!("layers.{i}.attention.wv.weight"), layer.wv.share());
                tensors.insert(format!("layers.{i}.attention.wo.weight"), layer.wo.share());

                tensors.insert(
                    format!("layers.{i}.ffn_norm.weight"),
                    layer.ffn_norm.share(),
                );

                tensors.insert(
                    format!("layers.{i}.feed_forward.w1.weight"),
                    layer.w1.share(),
                );
                tensors.insert(
                    format!("layers.{i}.feed_forward.w2.weight"),
                    layer.w2.share(),
                );
                tensors.insert(
                    format!("layers.{i}.feed_forward.w3.weight"),
                    layer.w3.share(),
                );

                layers.push(layer);
            }

            let n_mem = n_layer * n_ctx;
            let n_elements = n_embd * n_mem;
            let memory_k = context.new_tensor_1d(GGML_TYPE_F32, n_elements);
            let memory_v = context.new_tensor_1d(GGML_TYPE_F32, n_elements);

            let memory_size = memory_k.nbytes() + memory_v.nbytes();
            println!(
                "Memory size: {} MB {}",
                memory_size as f32 / 1024.0 / 1024.0,
                n_mem
            );

            LlamaModel {
                hparams,
                tok_embeddings,
                norm,
                output,
                layers,
                memory_k,
                memory_v,
                tensors,
                context,
            }
        };

        // Close the file, but keep its offset. That way we know how to skip the
        // metadata when loading the parts.
        let file_offset = reader.stream_position()?;
        drop(reader);

        for i in 0..n_parts {
            let part_id = i;

            let part_path = if i > 0 {
                path.join(format!(".{i}"))
            } else {
                path.to_path_buf()
            };
            let part_path_str = path.to_string_lossy();

            println!(
                "loading model part {}/{} from '{}'\n",
                i + 1,
                n_parts,
                part_path_str,
            );

            let mut part_reader = BufReader::new(File::open(part_path)?);
            // Skip metadata
            part_reader.seek(SeekFrom::Start(file_offset))?;

            let mut total_size = 0;
            let mut n_tensors = 0;

            // Load weights
            loop {
                if !part_reader.has_data_left()? {
                    break;
                }

                let n_dims = read_int(&mut part_reader)?;
                let length = read_int(&mut part_reader)?;
                let ftype = read_int(&mut part_reader)?;

                let mut ne = [1i32, 1i32];
                let mut nelements = 1;
                for i in 0..n_dims {
                    ne[i as usize] = read_int(&mut part_reader)?;
                    nelements *= ne[i as usize];
                }

                let tensor_name = read_string(&mut part_reader, length as usize)?;
                dbg!(&tensor_name);

                let Some(tensor) = model.tensors.get(&tensor_name)
                    else {
                        anyhow::bail!("Unknown tensor '{tensor_name}' in model_file '{part_path_str}'")
                    };

                #[allow(clippy::if_same_then_else)]
                let split_type = {
                    if tensor_name.contains("tok_embeddings") {
                        0
                    } else if tensor_name.contains("layers") {
                        if tensor_name.contains("attention.wo.weight") {
                            0
                        } else if tensor_name.contains("feed_forward.w2.weight") {
                            0
                        } else {
                            1
                        }
                    } else if tensor_name.contains("output") {
                        1
                    } else {
                        0
                    }
                };

                if n_dims == 1 {
                    if tensor.nelements() != nelements {
                        anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                    }
                } else {
                    if tensor.nelements() / n_parts != nelements {
                        anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                    }
                }

                if n_dims == 1 {
                    if tensor.get_ne()[0] != ne[0] || tensor.get_ne()[1] != ne[1] {
                        anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                    }
                } else {
                    if split_type == 0 {
                        if tensor.get_ne()[0] / n_parts != ne[0] || tensor.get_ne()[1] != ne[1] {
                            anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                        }
                    } else {
                        if tensor.get_ne()[0] != ne[0] || tensor.get_ne()[1] / n_parts != ne[1] {
                            anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                        }
                    }
                }

                fn ggml_type_size(t: ggml_type) -> usize {
                    unsafe { ggml_raw::ggml_type_size(t) }
                }

                fn ggml_blck_size(t: ggml_type) -> i32 {
                    unsafe { ggml_raw::ggml_blck_size(t) }
                }

                let bpe = match ftype {
                    0 => ggml_type_size(GGML_TYPE_F32),
                    1 => ggml_type_size(GGML_TYPE_F16),
                    2 => ggml_type_size(GGML_TYPE_Q4_0),
                    3 => ggml_type_size(GGML_TYPE_Q4_1),
                    _ => anyhow::bail!("Invalid ftype {ftype} in model file"),
                };

                if n_dims == 1 || n_parts == 1 {
                    if (nelements as usize * bpe) / ggml_blck_size(tensor.get_type()) as usize
                        != tensor.nbytes()
                    {
                        anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                    }

                    let data = tensor.data();

                    if part_id == 0 {
                        // SAFETY: yolo, same as original code
                        let slice = unsafe {
                            std::slice::from_raw_parts_mut(data as *mut u8, tensor.nbytes())
                        };
                        part_reader.read_exact(slice)?;
                    } else {
                        part_reader.seek(SeekFrom::Current(tensor.nbytes() as i64))?;
                    }

                    total_size += tensor.nbytes();
                } else {
                    if (nelements as usize * bpe) / ggml_blck_size(tensor.get_type()) as usize
                        != tensor.nbytes() / n_parts as usize
                    {
                        anyhow::bail!("Tensor {tensor_name} has the wrong size in model file");
                    }

                    if split_type == 0 {
                        let np0 = ne[0];
                        let row_size = (tensor.get_ne()[0] / ggml_blck_size(tensor.get_type()))
                            as usize
                            * ggml_type_size(tensor.get_type());

                        assert_eq!(row_size, tensor.get_nb()[1]);

                        for i1 in 0..ne[1] {
                            let offset_row = i1 as usize * row_size;
                            let offset = offset_row
                                + ((part_id * np0) as usize
                                    / ggml_blck_size(tensor.get_type()) as usize)
                                    * ggml_type_size(tensor.get_type());
                            // SAFETY: yolo, same as original code
                            unsafe {
                                let ptr = tensor.data().add(offset);
                                let slice = std::slice::from_raw_parts_mut(
                                    ptr as *mut u8,
                                    row_size / n_parts as usize,
                                );
                                part_reader.read_exact(slice)?;
                            }
                        }
                    } else {
                        let np1 = ne[1];
                        let row_size = (tensor.get_ne()[0] / ggml_blck_size(tensor.get_type()))
                            as usize
                            * ggml_type_size(tensor.get_type());

                        for i1 in 0..ne[1] {
                            let offset_row = (i1 + part_id * np1) as usize * row_size;
                            // SAFETY: yolo, same as original code
                            unsafe {
                                let ptr = tensor.data().add(offset_row);
                                let slice =
                                    std::slice::from_raw_parts_mut(ptr as *mut u8, row_size);
                                part_reader.read_exact(slice)?;
                            }
                        }
                    }

                    total_size += tensor.nbytes() / n_parts as usize
                }

                n_tensors += 1;
                if n_tensors % 8 == 0 {
                    print!(".");
                    io::stdout().flush()?;
                }
            }

            println!(" done");
            println!(
                "model size = {:.2} MB / num tensors = {}\n",
                total_size as f64 / 1024.0 / 1024.0,
                n_tensors
            );
        }

        Ok((model, vocab))
    }
}

fn main() {
    LlamaModel::load("/data/Llama/LLaMA/7B/ggml-model-q4_0.bin", 256)
        .expect("Could not load model");
}