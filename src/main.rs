mod layers;
mod models;

use models::{BertModel, Config};
use candle_transformers::models::bert::DTYPE;
use anyhow::{Error as E, Result};
use candle_core::{Tensor, Device};
use candle_nn::VarBuilder;
use clap::Parser;
use hf_hub::{api::sync::Api, Repo, RepoType};
use tokenizers::tokenizer::Tokenizer;
use std::collections::HashMap;
use serde::Deserialize;
use std::cmp::max;
use backend_core::{Batch, ModelType, Pool};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Run on CPU rather than on GPU.
    #[arg(long)]
    cpu: bool,

    /// Enable tracing (generates a trace-timestamp.json file).
    #[arg(long)]
    tracing: bool,

    /// The model to use, check out available models: https://huggingface.co/models?library=sentence-transformers&sort=trending
    #[arg(long)]
    model_id: Option<String>,

    #[arg(long)]
    revision: Option<String>,

    /// When set, compute embeddings for this prompt.
    #[arg(long)]
    prompt: Option<String>,

    /// Use the pytorch weights rather than the safetensors ones
    #[arg(long)]
    use_pth: bool,

    /// The number of times to run the prompt.
    #[arg(long, default_value = "1")]
    n: usize,

    /// L2 normalization for embeddings.
    #[arg(long, default_value = "true")]
    normalize_embeddings: bool,
}


fn device(cpu: bool) -> Result<Device> {
    Ok(Device::Cpu)
}

#[derive(Debug, Deserialize)]
pub struct ModelConfig {
    pub architectures: Vec<String>,
    pub model_type: String,
    #[serde(alias = "n_positions")]
    pub max_position_embeddings: usize,
    pub pad_token_id: usize,
    pub id2label: Option<HashMap<String, String>>,
    pub label2id: Option<HashMap<String, usize>>,
}

impl Args {
    fn build_model_and_tokenizer(&self) -> Result<(BertModel, Tokenizer)> {
        // let device = candle_examples::device(self.cpu)?;
        let device = device(self.cpu)?;
        let default_model = "intfloat/multilingual-e5-base".to_string();
        // let default_model = "sentence-transformers/all-MiniLM-L6-v2".to_string();
        let default_revision = "main".to_string();
        // let default_revision = "refs/pr/21".to_string();
        let (model_id, revision) = match (self.model_id.to_owned(), self.revision.to_owned()) {
            (Some(model_id), Some(revision)) => (model_id, revision),
            (Some(model_id), None) => (model_id, "main".to_string()),
            (None, Some(revision)) => (default_model, revision),
            (None, None) => (default_model, default_revision),
        };

        let repo = Repo::with_revision(model_id, RepoType::Model, revision);
        let (config_filename, tokenizer_filename, weights_filename) = {
            let api = Api::new()?;
            let api = api.repo(repo);
            let config = api.get("config.json")?;
            let tokenizer = api.get("tokenizer.json")?;
            let weights = if self.use_pth {
                api.get("pytorch_model.bin")?
            } else {
                api.get("model.safetensors")?
            };
            (config, tokenizer, weights)
        };
        let config = std::fs::read_to_string(config_filename)?;
        println!("config from JSON {}", &config);
        let config: Config = serde_json::from_str(&config)?;
        // Set pooling config
        let pool = Pool::Mean; // for intfloat/multilingual-e5-base
        let model_type = ModelType::Embedding(pool);
        let tokenizer = Tokenizer::from_file(tokenizer_filename).map_err(E::msg)?;

        let vb = if self.use_pth {
            VarBuilder::from_pth(&weights_filename, DTYPE, &device)?
        } else {
            unsafe { VarBuilder::from_mmaped_safetensors(&[weights_filename], DTYPE, &device)? }
        };
        println!("Starting model on CPU");
        let model = BertModel::load(vb, &config, model_type)?;
        Ok((model, tokenizer))
    }
}

fn main() -> Result<()> {
    use tracing_chrome::ChromeLayerBuilder;
    use tracing_subscriber::prelude::*;

    let args = Args::parse();
    let _guard = if args.tracing {
        println!("tracing...");
        let (chrome_layer, guard) = ChromeLayerBuilder::new().build();
        tracing_subscriber::registry().with(chrome_layer).init();
        Some(guard)
    } else {
        None
    };
    let start = std::time::Instant::now();

    let (model, mut tokenizer) = args.build_model_and_tokenizer()?;

    
    let sentences = [
        "This framework generates embeddings for each input sentence",
        "This framework generates embeddings for each input sentence",
    ];

    // let sentences = [
    //     // "The cat sits outside",
    //     // "A man is playing guitar",
    //     // "I love pasta",
    //     // "The new movie is awesome",
    //     // "The cat plays in the garden",
    //     // "A woman watches TV",
    //     // "The new movie is so great",
    //     // "Do you like pizza?",
    // ];
    let tokenizer = tokenizer
        .with_padding(None)
        .with_truncation(None)
        .map_err(E::msg)?;
    
    let encodings = tokenizer
        .encode_batch(sentences.to_vec(), true)
        .map_err(E::msg)?;

    let capacity = 100;
    let max_batch_tokens = 1000;
    let mut input_ids = Vec::with_capacity(max_batch_tokens);
    let mut token_type_ids = Vec::with_capacity(max_batch_tokens);
    let mut position_ids = Vec::with_capacity(max_batch_tokens);
    let mut cu_seq_lengths = Vec::with_capacity(capacity);
    cu_seq_lengths.push(0);
    let mut current_tokens = 0;
    let mut max_length = 0;

    let position_offset = 2; // for roberta
    for encoding in encodings {
        let seq_len = encoding.len();
        input_ids.extend(encoding.get_ids().to_vec());
        token_type_ids.extend(encoding.get_type_ids().to_vec());
        position_ids.extend((position_offset as u32..(seq_len + position_offset) as u32)
        .collect::<Vec<_>>(),);
    
        let entry_tokens = encoding.get_ids().to_vec().len();
        current_tokens += entry_tokens;
        max_length = max(max_length, entry_tokens as u32);
        cu_seq_lengths.push(current_tokens as u32);
    }

    let batch = Batch {
        input_ids,
        token_type_ids,
        position_ids,
        cumulative_seq_lengths: cu_seq_lengths,
        max_length,
    };

    println!("constructed batch from input");
    let ys = model.forward(batch)?;

    let embeddings =  normalize_l2(&ys)?;
    println!("pooled embeddings {embeddings}");
    // dbg!(embeddings);

    println!("Took {:?}", start.elapsed()); //to_vecX()

   
    Ok(())
}

pub fn normalize_l2(v: &Tensor) -> Result<Tensor> {
    Ok(v.broadcast_div(&v.sqr()?.sum_keepdim(1)?.sqrt()?)?)
}