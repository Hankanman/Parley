use std::io::{self, BufRead, Write};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::pin::pin;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use encoding_rs;
use llama_cpp_2::context::params::LlamaContextParams;
use llama_cpp_2::llama_backend::LlamaBackend;
use llama_cpp_2::llama_batch::LlamaBatch;
use llama_cpp_2::model::params::LlamaModelParams;
use llama_cpp_2::model::{AddBos, LlamaModel};
// Protocol messages (JSON over stdin/stdout) are shared with the app's
// summary engine (`parley-core`'s `summary_engine::sidecar`) via the
// llama-protocol crate so the two ends can't drift apart.
use llama_protocol::{Request, Response};

// ============================================================================
// VRAM Detection and GPU Layer Calculation
// ============================================================================

/// Detect available VRAM in GB
fn detect_vram_gb() -> f32 {
    #[cfg(feature = "cuda")]
    {
        // NVIDIA CUDA: Query device memory
        if let Some(vram) = detect_cuda_vram() {
            eprintln!("CUDA VRAM detected: {:.2} GB", vram);
            return vram;
        }
    }

    // TODO: Vulkan VRAM detection

    eprintln!("VRAM detection not available, using conservative estimate");
    4.0 // Conservative fallback
}

#[cfg(feature = "cuda")]
fn detect_cuda_vram() -> Option<f32> {
    // Use nvidia-smi to query VRAM
    if let Ok(output) = std::process::Command::new("nvidia-smi")
        .args(&["--query-gpu=memory.free", "--format=csv,noheader,nounits"])
        .output()
    {
        if let Ok(stdout) = String::from_utf8(output.stdout) {
            if let Ok(mb) = stdout.trim().parse::<f32>() {
                return Some(mb / 1024.0); // Convert MB to GB
            }
        }
    }
    None
}

/// Calculate safe GPU layer count based on VRAM, model file size, and context size
fn calculate_gpu_layers(
    model_path: &PathBuf,
    model_layers: u32,
    vram_gb: f32,
    context_size: u32,
) -> u32 {
    let file_size_gb = std::fs::metadata(model_path)
        .map(|m| m.len() as f32 / 1024.0 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    if file_size_gb == 0.0 {
        eprintln!("⚠️ Could not determine model file size, using conservative default");
        return 0;
    }

    // Heuristic: Estimate KV cache size
    // 7B models (approx > 2.5GB) usually have 4096 hidden dim -> ~256MB per 1k context
    // 1B models (approx < 2.5GB) usually have 2048 hidden dim -> ~128MB per 1k context
    let kv_per_1k_gb = if file_size_gb > 2.5 { 0.25 } else { 0.12 };
    let total_kv_gb = (context_size as f32 / 1000.0) * kv_per_1k_gb;

    // Safety buffer (500MB) for OS/Display
    let safe_vram = vram_gb - 0.5;

    // For debugging
    eprintln!("📊 VRAM Analysis:");
    eprintln!("   • Available: {:.2} GB", vram_gb);
    eprintln!("   • Safe Limit: {:.2} GB", safe_vram);
    eprintln!("   • Model Weights: {:.2} GB", file_size_gb);
    eprintln!(
        "   • KV Cache ({} ctx): {:.2} GB",
        context_size, total_kv_gb
    );

    if safe_vram <= 0.0 {
        eprintln!("⚠️ No safe VRAM available, using CPU only");
        return 0;
    }

    // Calculate cost per layer
    let weight_per_layer = file_size_gb / model_layers as f32;
    let kv_per_layer = total_kv_gb / model_layers as f32;
    let total_per_layer = weight_per_layer + kv_per_layer;

    // Calculate how many layers fit
    let safe_layers = (safe_vram / total_per_layer).floor() as u32;
    let layers = safe_layers.min(model_layers);

    eprintln!(
        "   • Cost per layer: {:.2} MB (Weights) + {:.2} MB (KV) = {:.2} MB",
        weight_per_layer * 1024.0,
        kv_per_layer * 1024.0,
        total_per_layer * 1024.0
    );

    if layers < model_layers {
        eprintln!(
            "⚠️ Memory constrained. Offloading {}/{} layers ({:.1}%)",
            layers,
            model_layers,
            (layers as f32 / model_layers as f32) * 100.0
        );
    } else {
        eprintln!("✅ Full offload possible ({} layers)", layers);
    }

    layers
}

/// Get default GPU layer count with smart detection
fn get_default_gpu_layers(model_path: &PathBuf, context_size: u32) -> u32 {
    let vram = detect_vram_gb();
    // TODO: Use actual model metadata instead of heuristics
    // Heuristic: Estimate total layers based on file size
    // 7B models (Q4) are ~4.1GB and have ~32-35 layers
    // 1B models (Q4) are ~1.1GB and have ~20-28 layers
    let file_size_gb = std::fs::metadata(model_path)
        .map(|m| m.len() as f32 / 1024.0 / 1024.0 / 1024.0)
        .unwrap_or(0.0);

    let estimated_layers = if file_size_gb > 2.5 { 33 } else { 28 };

    calculate_gpu_layers(model_path, estimated_layers, vram, context_size)
}

// ============================================================================
// Model State Management
// ============================================================================

struct ModelState {
    backend: LlamaBackend,
    model: Option<LlamaModel>,
    model_path: Option<PathBuf>,
    context_size: u32,
}

impl ModelState {
    fn new() -> Result<Self> {
        let backend = LlamaBackend::init().context("Failed to init LlamaBackend")?;
        Ok(Self {
            backend,
            model: None,
            model_path: None,
            context_size: 2048,
        })
    }

    fn load_model_if_needed(&mut self, model_path: PathBuf, context_size: u32) -> Result<()> {
        // Check if model is already loaded
        if let Some(ref loaded_path) = self.model_path {
            if loaded_path == &model_path && self.context_size == context_size {
                eprintln!("✓ Model already loaded");
                return Ok(());
            }
        }

        eprintln!("📥 Loading model: {}", model_path.display());

        // Detect GPU layers
        let gpu_layers = get_default_gpu_layers(&model_path, context_size);

        // Configure model parameters with GPU offload
        let model_params = LlamaModelParams::default().with_n_gpu_layers(gpu_layers);
        let model_params = pin!(model_params);

        let model = LlamaModel::load_from_file(&self.backend, model_path.clone(), &model_params)
            .with_context(|| format!("unable to load model at {:?}", model_path))?;

        self.model = Some(model);
        self.model_path = Some(model_path);
        self.context_size = context_size;

        eprintln!("✅ Model loaded successfully");
        Ok(())
    }

    fn generate(
        &mut self,
        prompt: String,
        max_tokens: i32,
        temperature: f32,
        top_k: i32,
        top_p: f32,
        stop_tokens: Vec<String>,
        json_schema: Option<String>,
    ) -> Result<String> {
        let start_time = Instant::now();
        let model = self.model.as_ref().context("Model not loaded")?;

        // Calculate thread count (conservative default: max(1, (Cores / 2) + 2))
        // This ensures the UI thread is never starved
        let threads: i32 = std::thread::available_parallelism()
            .map(|n| {
                let cores = n.get() as i32;
                ((cores / 2) + 2).max(1)
            })
            .unwrap_or(2);

        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(Some(
                NonZeroU32::new(self.context_size).context("Invalid ctx size")?,
            ))
            .with_n_batch(self.context_size)
            .with_n_threads(threads)
            .with_n_threads_batch(threads);

        let mut ctx = model
            .new_context(&self.backend, ctx_params)
            .context("unable to create the llama_context")?;

        let tokens_list = model
            .str_to_token(&prompt, AddBos::Always)
            .with_context(|| "failed to tokenize prompt")?;

        eprintln!("📝 Tokenized prompt: {} tokens", tokens_list.len());

        // Use context size for batch capacity to handle long prompts
        let batch_size = self.context_size as usize;
        let mut batch = LlamaBatch::new(batch_size, 1);

        let last_index: i32 = (tokens_list.len() - 1) as i32;
        for (i, token) in (0_i32..).zip(tokens_list.into_iter()) {
            let is_last = i == last_index;
            batch
                .add(token, i, &[0], is_last)
                .context("Failed to add token to batch")?;
        }

        ctx.decode(&mut batch).context("llama_decode() failed")?;
        let prompt_time = start_time.elapsed();

        let n_prompt_tokens = batch.n_tokens();
        let mut n_cur = n_prompt_tokens;
        let mut decoder = encoding_rs::UTF_8.new_decoder();
        let mut output = String::new();

        eprintln!("🔄 Starting generation (max_tokens: {})", max_tokens);

        use llama_cpp_2::sampling::LlamaSampler;

        // Build the sampler chain ONCE for the whole generation — the chain
        // (including the dist sampler's RNG) is stateful and designed to be
        // reused across tokens; rebuilding it per token wastes work.
        let mut samplers: Vec<LlamaSampler> = Vec::with_capacity(5);

        // Optional JSON-schema constraint: converted to a GBNF grammar and
        // placed FIRST in the chain, so every later sampler only ever sees
        // tokens that keep the output valid against the schema.
        if let Some(schema) = json_schema.as_deref() {
            let grammar = llama_cpp_2::json_schema_to_grammar(schema)
                .map_err(|e| anyhow::anyhow!("invalid JSON schema: {e}"))?;
            let grammar_sampler = LlamaSampler::grammar(model, &grammar, "root")
                .map_err(|e| anyhow::anyhow!("failed to build grammar sampler: {e}"))?;
            eprintln!(
                "🧩 Constraining output to a JSON schema ({} byte grammar)",
                grammar.len()
            );
            samplers.push(grammar_sampler);
        }

        if temperature <= 0.0 {
            // Greedy sampling for temp <= 0
            samplers.push(LlamaSampler::greedy());
        } else {
            // Random sampling with temperature/top_k/top_p
            let seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u32;

            samplers.extend([
                LlamaSampler::top_k(top_k),
                LlamaSampler::top_p(top_p, 1),
                LlamaSampler::temp(temperature),
                LlamaSampler::dist(seed),
            ]);
        }
        let sampler = LlamaSampler::chain_simple(samplers);
        let mut sampler = pin!(sampler);

        // Longest stop token, for bounding the per-token stop scan to the
        // output's tail instead of rescanning the whole string every token.
        let max_stop_len = stop_tokens.iter().map(|s| s.len()).max().unwrap_or(0);

        // Streaming: emit `chunk` lines as text accumulates, batched to
        // STREAM_BATCH_BYTES so the pipe isn't hit once per token. The last
        // `max_stop_len` bytes are always held back — they might turn out to
        // be the start of a stop token, which must never reach the client.
        // The terminal `response` line carries the full final text either way.
        const STREAM_BATCH_BYTES: usize = 64;
        let mut streamed_len = 0usize;

        loop {
            // Check if we've generated enough tokens
            if (n_cur - n_prompt_tokens) >= max_tokens {
                eprintln!("✓ Reached max_tokens limit");
                break;
            }

            let token = sampler.as_mut().sample(&ctx, batch.n_tokens() - 1);
            // No explicit `accept`: `sample` (llama_sampler_sample) already
            // accepts the token into every sampler in the chain. Accepting
            // twice advances stateful samplers twice — fatal for the grammar
            // sampler (GGML_ASSERT(!stacks.empty()) in llama-grammar.cpp).

            if model.is_eog_token(token) {
                eprintln!(
                    "✓ End-of-generation token reached (generated {} chars)",
                    output.len()
                );
                break;
            }

            // `true` is the boolean equivalent of `Special::Tokenize` (render
            // special tokens as their textual form rather than skipping them).
            // Token pieces can exceed any fixed buffer (Gemma 3's tokenizer
            // has pieces up to 48 bytes); on InsufficientBufferSpace llama.cpp
            // reports the required size as a negative value, so retry with it.
            let output_bytes = match model.token_to_piece_bytes(token, 32, true, None) {
                Ok(bytes) => bytes,
                Err(llama_cpp_2::TokenToStringError::InsufficientBufferSpace(needed)) => model
                    .token_to_piece_bytes(token, needed.unsigned_abs() as usize, true, None)
                    .context("Failed to convert token to bytes (after resize)")?,
                Err(e) => return Err(e).context("Failed to convert token to bytes"),
            };

            let mut token_text = String::with_capacity(32);
            let _ = decoder.decode_to_string(&output_bytes, &mut token_text, false);
            output.push_str(&token_text);

            // Check for model-specific stop tokens. Only the tail can contain
            // a *new* occurrence (earlier text was already scanned), so look
            // back just far enough to catch one spanning the boundary of the
            // freshly appended piece.
            let mut should_stop = false;
            if max_stop_len > 0 {
                let mut scan_from = output.len().saturating_sub(token_text.len() + max_stop_len);
                while scan_from > 0 && !output.is_char_boundary(scan_from) {
                    scan_from -= 1;
                }
                for stop_token in &stop_tokens {
                    if let Some(pos) = output[scan_from..].find(stop_token.as_str()) {
                        eprintln!(
                            "✓ Stop token '{}' detected (generated {} chars)",
                            stop_token,
                            output.len()
                        );
                        // Cut at the stop token — anything after it is noise.
                        output.truncate(scan_from + pos);
                        output.truncate(output.trim_end().len());
                        should_stop = true;
                        break;
                    }
                }
            }
            if should_stop {
                break;
            }

            // Stream the newly safe (can't-be-a-stop-token) prefix.
            let mut safe_len = output.len().saturating_sub(max_stop_len);
            while safe_len > 0 && !output.is_char_boundary(safe_len) {
                safe_len -= 1;
            }
            if safe_len > streamed_len && safe_len - streamed_len >= STREAM_BATCH_BYTES {
                send_response(&Response::Chunk {
                    text: output[streamed_len..safe_len].to_string(),
                })?;
                streamed_len = safe_len;
            }

            batch.clear();
            batch
                .add(token, n_cur, &[0], true)
                .context("Failed to add generated token to batch")?;
            n_cur += 1;
            ctx.decode(&mut batch).context("failed to eval")?;
        }

        // Generation statistics
        let total_time = start_time.elapsed();
        let gen_time = total_time.saturating_sub(prompt_time);
        let output_tokens = (n_cur - n_prompt_tokens) as u64;
        let prompt_tokens = n_prompt_tokens as u64;

        let tokens_per_sec = if gen_time.as_secs_f64() > 0.0 {
            output_tokens as f64 / gen_time.as_secs_f64()
        } else {
            0.0
        };

        eprintln!("📊 Generation Statistics:");
        eprintln!("   • Prompt tokens: {}", prompt_tokens);
        eprintln!("   • Output tokens: {}", output_tokens);
        eprintln!("   • Prompt processing: {:.2}s", prompt_time.as_secs_f64());
        eprintln!("   • Generation time: {:.2}s", gen_time.as_secs_f64());
        eprintln!("   • Total time: {:.2}s", total_time.as_secs_f64());
        eprintln!("   • Speed: {:.2} tokens/sec", tokens_per_sec);

        Ok(output)
    }
}

// ============================================================================
// Embeddings
// ============================================================================

/// Context window for embedding requests. Parley embeds transcript chunks of
/// about 800 characters and short queries, far inside this; longer texts are
/// truncated to it.
const EMBED_CONTEXT: u32 = 2048;
/// Most texts packed into one decode (each is its own sequence).
const EMBED_MAX_SEQS: usize = 32;

impl ModelState {
    /// One pooled embedding per text. Texts are packed into as few decodes
    /// as fit in the batch, one sequence each.
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let model = self.model.as_ref().context("Model not loaded")?;
        let n_ctx = EMBED_CONTEXT.min(model.n_ctx_train().max(512));
        let threads: i32 = std::thread::available_parallelism()
            .map(|n| ((n.get() as i32 / 2) + 2).max(1))
            .unwrap_or(2);
        // Embedding models attend bidirectionally, so a whole sequence must
        // fit in one micro-batch: n_ubatch = n_batch = n_ctx.
        let ctx_params = LlamaContextParams::default()
            .with_n_ctx(NonZeroU32::new(n_ctx))
            .with_n_batch(n_ctx)
            .with_n_ubatch(n_ctx)
            .with_n_threads(threads)
            .with_n_threads_batch(threads)
            .with_n_seq_max(texts.len().clamp(1, EMBED_MAX_SEQS) as u32)
            .with_embeddings(true);
        let mut ctx = model
            .new_context(&self.backend, ctx_params)
            .context("unable to create the embedding context")?;

        let mut tokenized = Vec::with_capacity(texts.len());
        for text in texts {
            let mut tokens = model
                .str_to_token(text, AddBos::Always)
                .context("failed to tokenize text for embedding")?;
            tokens.truncate(n_ctx as usize);
            if tokens.is_empty() {
                anyhow::bail!("nothing to embed in an empty text");
            }
            tokenized.push(tokens);
        }

        let mut vectors = Vec::with_capacity(texts.len());
        let mut batch = LlamaBatch::new(n_ctx as usize, 1);
        let mut pending = 0usize;
        let flush = |batch: &mut LlamaBatch,
                         ctx: &mut llama_cpp_2::context::LlamaContext,
                         count: usize,
                         vectors: &mut Vec<Vec<f32>>|
         -> Result<()> {
            ctx.clear_kv_cache();
            ctx.decode(batch).context("llama_decode() failed while embedding")?;
            for seq in 0..count {
                let v = ctx
                    .embeddings_seq_ith(seq as i32)
                    .map_err(|e| anyhow::anyhow!("no embedding for sequence {seq}: {e}"))?;
                vectors.push(v.to_vec());
            }
            batch.clear();
            Ok(())
        };
        for tokens in &tokenized {
            if pending > 0
                && (pending == EMBED_MAX_SEQS || batch.n_tokens() as usize + tokens.len() > n_ctx as usize)
            {
                flush(&mut batch, &mut ctx, pending, &mut vectors)?;
                pending = 0;
            }
            batch
                .add_sequence(tokens, pending as i32, false)
                .context("failed to add text to the embedding batch")?;
            pending += 1;
        }
        if pending > 0 {
            flush(&mut batch, &mut ctx, pending, &mut vectors)?;
        }
        Ok(vectors)
    }
}

// ============================================================================
// Main Loop with Keep-Alive Protocol
// ============================================================================

fn send_response(response: &Response) -> Result<()> {
    let json = serde_json::to_string(response)?;
    println!("{}", json);
    io::stdout().flush()?;
    Ok(())
}

fn main() -> Result<()> {
    // Rust starts with SIGPIPE ignored, so writing our JSON response to a
    // stdout whose reader (the Parley app) has gone away returns EPIPE and
    // `println!` panics with "failed printing to stdout: Broken pipe". Restore
    // the default disposition so a vanished parent just terminates us quietly —
    // the idiomatic behavior for a pipe-writing helper. Safe: this is the first
    // thing main does, before any threads exist.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    // Idle shutdown is the PARENT's job (see SidecarManager's idle loop):
    // this process spends its life blocked in read_line below, so a timer
    // here could only ever be checked between requests — and an unsolicited
    // "Goodbye" line would desync the parent's next response read. We simply
    // serve until stdin closes or a shutdown request arrives.
    eprintln!("🦙 llama-helper starting");

    let mut state = ModelState::new()?;

    let stdin = io::stdin();
    let mut stdin_lock = stdin.lock();
    let mut buffer = String::new();

    loop {
        // Read line from stdin
        buffer.clear();
        match stdin_lock.read_line(&mut buffer) {
            Ok(0) => {
                // EOF reached
                eprintln!("📪 EOF received, shutting down");
                break;
            }
            Ok(_) => {
                let line = buffer.trim();
                if line.is_empty() {
                    continue;
                }

                // Parse request
                match serde_json::from_str::<Request>(line) {
                    Ok(Request::Generate {
                        prompt,
                        max_tokens,
                        context_size,
                        model_path,
                        temperature,
                        top_k,
                        top_p,
                        stop_tokens,
                        json_schema,
                    }) => {
                        let max_tokens = max_tokens.unwrap_or(512);
                        let context_size = context_size.unwrap_or(2048);

                        // Sampling parameters with sensible defaults
                        let temperature = temperature.unwrap_or(1.0);
                        let top_k = top_k.unwrap_or(64);
                        let top_p = top_p.unwrap_or(0.95);
                        let stop_tokens = stop_tokens.unwrap_or_else(Vec::new);

                        // Load model if path provided
                        if let Some(path_str) = model_path {
                            let path = PathBuf::from(path_str);
                            if let Err(e) = state.load_model_if_needed(path, context_size) {
                                send_response(&Response::Response {
                                    text: String::new(),
                                    error: Some(format!("Failed to load model: {}", e)),
                                })?;
                                continue;
                            }
                        }

                        // Generate response with sampling parameters
                        match state.generate(
                            prompt,
                            max_tokens,
                            temperature,
                            top_k,
                            top_p,
                            stop_tokens,
                            json_schema,
                        ) {
                            Ok(text) => {
                                send_response(&Response::Response { text, error: None })?;
                            }
                            Err(e) => {
                                send_response(&Response::Response {
                                    text: String::new(),
                                    error: Some(format!("Generation failed: {}", e)),
                                })?;
                            }
                        }
                    }
                    Ok(Request::Embed { model_path, texts }) => {
                        let result = state
                            .load_model_if_needed(PathBuf::from(model_path), EMBED_CONTEXT)
                            .and_then(|()| state.embed(&texts));
                        match result {
                            Ok(vectors) => send_response(&Response::Embeddings { vectors })?,
                            Err(e) => send_response(&Response::Error {
                                message: format!("Embedding failed: {e:#}"),
                            })?,
                        }
                    }
                    Ok(Request::Ping) => {
                        send_response(&Response::Pong)?;
                    }
                    Ok(Request::Shutdown) => {
                        eprintln!("🛑 Shutdown requested");
                        send_response(&Response::Goodbye)?;
                        break;
                    }
                    Err(e) => {
                        eprintln!("❌ Failed to parse request: {}", e);
                        send_response(&Response::Error {
                            message: format!("Invalid request: {}", e),
                        })?;
                    }
                }
            }
            Err(e) => {
                eprintln!("❌ Error reading stdin: {}", e);
                break;
            }
        }
    }

    eprintln!("👋 llama-helper exiting");
    Ok(())
}
