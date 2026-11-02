#[cfg(target_os = "macos")]
#[allow(deprecated, unsafe_op_in_unsafe_fn)]
mod platform {
    use std::collections::BTreeMap;
    use std::fs::File;
    use std::panic::AssertUnwindSafe;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex, PoisonError};

    use half::f16;
    use hf_hub::api::sync::Api;
    use memmap2::{Mmap, MmapOptions};
    use objc2::AnyThread;
    use objc2::rc::{Retained, autoreleasepool};
    use objc2::runtime::{AnyObject, ProtocolObject};
    use objc2_core_ml::{
        MLComputeUnits, MLDictionaryFeatureProvider, MLFeatureProvider, MLFeatureValue, MLModel,
        MLModelConfiguration, MLMultiArray, MLMultiArrayDataType,
    };
    use objc2_foundation::{NSArray, NSDictionary, NSError, NSNumber, NSString, NSURL};
    use tokenizers::Tokenizer;

    const REPO: &str = "FluidInference/embeddinggemma-2-coreml";
    const PACKAGE: &str = "EmbeddingGemma2Text.mlpackage";
    const TABLE: &str = "embeddings.bf16";
    const TOKENIZER: &str = "tokenizer.json";
    const CONFIG: &str = "config.json";
    const PACKAGE_FILES: [&str; 3] = [
        "EmbeddingGemma2Text.mlpackage/Manifest.json",
        "EmbeddingGemma2Text.mlpackage/Data/com.apple.CoreML/model.mlmodel",
        "EmbeddingGemma2Text.mlpackage/Data/com.apple.CoreML/weights/weight.bin",
    ];
    const BUCKETS: [usize; 6] = [32, 48, 64, 128, 256, 512];
    const HIDDEN: usize = 512;
    const DIMENSIONS: usize = 768;
    const VOCABULARY: usize = 262_144;
    const PACK_LENGTH: usize = 256;
    const PACK_SLOTS: usize = 8;

    struct LoadedModel(Retained<MLModel>);

    // Core ML documents prediction as thread-safe. Access is additionally serialized by model mutexes.
    unsafe impl Send for LoadedModel {}

    pub struct EmbeddingGemma2 {
        tokenizer: Tokenizer,
        table: Mmap,
        scale: f32,
        pad_id: u32,
        compiled: PathBuf,
        models: Mutex<BTreeMap<usize, LoadedModel>>,
        pack: Arc<Mutex<Option<LoadedModel>>>,
        pack_started: AtomicBool,
    }

    impl EmbeddingGemma2 {
        pub fn load() -> Result<Self, String> {
            let repo = Api::new()
                .map_err(|error| error.to_string())?
                .model(REPO.to_string());
            for file in PACKAGE_FILES {
                repo.get(file).map_err(|error| error.to_string())?;
            }
            let table_path = repo.get(TABLE).map_err(|error| error.to_string())?;
            let tokenizer_path = repo.get(TOKENIZER).map_err(|error| error.to_string())?;
            let config_path = repo.get(CONFIG).map_err(|error| error.to_string())?;
            let root = config_path
                .parent()
                .ok_or_else(|| "EmbeddingGemma 2 cache has no root directory".to_string())?;
            let revision = root
                .file_name()
                .and_then(|value| value.to_str())
                .unwrap_or("current");
            let compiled = compiled_path(revision)?;
            if !compiled.is_dir() {
                tracing::info!("compiling EmbeddingGemma 2 for Apple Neural Engine");
                compile_into(&root.join(PACKAGE), &compiled)?;
            }

            let config: serde_json::Value = serde_json::from_slice(
                &std::fs::read(config_path).map_err(|error| error.to_string())?,
            )
            .map_err(|error| error.to_string())?;
            let hidden = config["hidden_size"]
                .as_u64()
                .ok_or_else(|| "EmbeddingGemma 2 config has no hidden_size".to_string())?
                as usize;
            let dimensions = config["embedding_dim"]
                .as_u64()
                .ok_or_else(|| "EmbeddingGemma 2 config has no embedding_dim".to_string())?
                as usize;
            if hidden != HIDDEN || dimensions != DIMENSIONS {
                return Err(format!(
                    "unexpected EmbeddingGemma 2 shape: hidden={hidden}, dimensions={dimensions}"
                ));
            }
            let scale = config["embed_scale"]
                .as_f64()
                .ok_or_else(|| "EmbeddingGemma 2 config has no embed_scale".to_string())?
                as f32;
            let file = File::open(table_path).map_err(|error| error.to_string())?;
            let table =
                unsafe { MmapOptions::new().map(&file) }.map_err(|error| error.to_string())?;
            let expected = VOCABULARY * HIDDEN * 2;
            if table.len() != expected {
                return Err(format!(
                    "unexpected EmbeddingGemma 2 token table size: {} (expected {expected})",
                    table.len()
                ));
            }
            let mut tokenizer =
                Tokenizer::from_file(tokenizer_path).map_err(|error| error.to_string())?;
            let pad_id = tokenizer.token_to_id("<pad>").unwrap_or(0);
            tokenizer.with_padding(None);
            Ok(Self {
                tokenizer,
                table,
                scale,
                pad_id,
                compiled,
                models: Mutex::new(BTreeMap::new()),
                pack: Arc::new(Mutex::new(None)),
                pack_started: AtomicBool::new(false),
            })
        }

        pub fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            if texts.is_empty() {
                return Ok(Vec::new());
            }
            let ids: Vec<Vec<u32>> = texts
                .iter()
                .map(|text| {
                    self.tokenizer
                        .encode(text.as_str(), true)
                        .map(|encoding| encoding.get_ids().iter().copied().take(512).collect())
                        .map_err(|error| error.to_string())
                })
                .collect::<Result<_, _>>()?;
            let (bins, mut singles) = plan_packs(&ids.iter().map(Vec::len).collect::<Vec<_>>());
            let mut output = vec![None; ids.len()];
            let pack_ready = self.pack.try_lock().is_ok_and(|pack| pack.is_some());
            let start_pack = !pack_ready && !bins.is_empty();
            if pack_ready {
                self.embed_packed(&ids, &bins, &mut output)?;
            } else {
                singles.extend(bins.into_iter().flatten());
                singles.sort_unstable();
            }
            if !singles.is_empty() {
                self.embed_singles(&ids, &singles, &mut output)?;
            }
            if start_pack {
                self.start_pack_load();
            }
            output
                .into_iter()
                .map(|vector| vector.ok_or_else(|| "EmbeddingGemma 2 returned no vector".into()))
                .collect()
        }

        fn embed_singles(
            &self,
            ids: &[Vec<u32>],
            indices: &[usize],
            output: &mut [Option<Vec<f32>>],
        ) -> Result<(), String> {
            let longest = indices
                .iter()
                .map(|&index| ids[index].len())
                .max()
                .unwrap_or(0);
            let bucket = BUCKETS
                .iter()
                .copied()
                .find(|&size| size >= longest)
                .ok_or_else(|| "EmbeddingGemma 2 input exceeds 512 tokens".to_string())?;
            let mut models = self.models.lock().unwrap_or_else(PoisonError::into_inner);
            if !models.contains_key(&bucket) {
                models.insert(
                    bucket,
                    LoadedModel(load_function(&self.compiled, &format!("embed_{bucket}"))?),
                );
            }
            for &index in indices {
                let (embeddings, mask) = self.inputs(&ids[index], bucket)?;
                output[index] = Some(autoreleasepool(|_| unsafe {
                    let embeddings = new_f16_array(&[1, bucket, HIDDEN], &embeddings)?;
                    let mask = new_f16_array(&[1, bucket], &mask)?;
                    let result = predict(
                        &models[&bucket].0,
                        &[("inputs_embeds", &embeddings), ("attention_mask", &mask)],
                    )?;
                    read_embedding(&result, DIMENSIONS)
                })?);
            }
            Ok(())
        }

        fn embed_packed(
            &self,
            ids: &[Vec<u32>],
            bins: &[Vec<usize>],
            output: &mut [Option<Vec<f32>>],
        ) -> Result<(), String> {
            let pack = self.pack.lock().unwrap_or_else(PoisonError::into_inner);
            let model = &pack.as_ref().expect("packed model was loaded").0;
            for bin in bins {
                let mut combined = Vec::with_capacity(PACK_LENGTH);
                let mut bias = vec![f16::from_f32(-10_000.0); PACK_LENGTH * PACK_LENGTH];
                let mut positions = vec![f16::ZERO; PACK_LENGTH];
                let mut pool = vec![f16::ZERO; PACK_SLOTS * PACK_LENGTH];
                for (slot, &index) in bin.iter().enumerate() {
                    let start = combined.len();
                    combined.extend_from_slice(&ids[index]);
                    let end = combined.len();
                    let weight = f16::from_f32(1.0 / ids[index].len() as f32);
                    for row in start..end {
                        bias[row * PACK_LENGTH + start..row * PACK_LENGTH + end].fill(f16::ZERO);
                        positions[row] = f16::from_f32((row - start) as f32);
                        pool[slot * PACK_LENGTH + row] = weight;
                    }
                }
                for row in combined.len()..PACK_LENGTH {
                    bias[row * PACK_LENGTH + row] = f16::ZERO;
                }
                let embeddings = self.embeddings(&combined, PACK_LENGTH)?;
                let values = autoreleasepool(|_| unsafe {
                    let embeddings = new_f16_array(&[1, PACK_LENGTH, HIDDEN], &embeddings)?;
                    let bias = new_f16_array(&[1, 1, PACK_LENGTH, PACK_LENGTH], &bias)?;
                    let positions = new_f16_array(&[PACK_LENGTH, 1], &positions)?;
                    let pool = new_f16_array(&[PACK_SLOTS, PACK_LENGTH], &pool)?;
                    let result = predict(
                        model,
                        &[
                            ("inputs_embeds", &embeddings),
                            ("attention_bias", &bias),
                            ("positions", &positions),
                            ("pool", &pool),
                        ],
                    )?;
                    read_embedding(&result, PACK_SLOTS * DIMENSIONS)
                })?;
                for (slot, &index) in bin.iter().enumerate() {
                    output[index] =
                        Some(values[slot * DIMENSIONS..(slot + 1) * DIMENSIONS].to_vec());
                }
            }
            Ok(())
        }

        fn inputs(&self, ids: &[u32], bucket: usize) -> Result<(Vec<f16>, Vec<f16>), String> {
            let embeddings = self.embeddings(ids, bucket)?;
            let mask = (0..bucket)
                .map(|index| f16::from_f32(if index < ids.len() { 1.0 } else { 0.0 }))
                .collect();
            Ok((embeddings, mask))
        }

        fn embeddings(&self, ids: &[u32], length: usize) -> Result<Vec<f16>, String> {
            let mut embeddings = Vec::with_capacity(length * HIDDEN);
            for index in 0..length {
                let id = ids.get(index).copied().unwrap_or(self.pad_id) as usize;
                if id >= VOCABULARY {
                    return Err(format!("token id {id} is outside the vocabulary"));
                }
                let offset = id * HIDDEN * 2;
                for bytes in self.table[offset..offset + HIDDEN * 2].chunks_exact(2) {
                    let bits = u16::from_le_bytes([bytes[0], bytes[1]]) as u32;
                    embeddings.push(f16::from_f32(f32::from_bits(bits << 16) * self.scale));
                }
            }
            Ok(embeddings)
        }

        // Keep the first semantic request on the smaller single-text function while this loads.
        fn start_pack_load(&self) {
            if self.pack_started.swap(true, Ordering::Relaxed) {
                return;
            }
            let compiled = self.compiled.clone();
            let pack = Arc::clone(&self.pack);
            std::thread::spawn(move || match load_function(&compiled, "pack_256") {
                Ok(model) => {
                    *pack.lock().unwrap_or_else(PoisonError::into_inner) = Some(LoadedModel(model));
                }
                Err(error) => tracing::warn!(%error, "packed semantic model unavailable"),
            });
        }
    }

    fn load_function(compiled: &Path, name: &str) -> Result<Retained<MLModel>, String> {
        autoreleasepool(|_| unsafe {
            let configuration = MLModelConfiguration::new();
            configuration.setComputeUnits(MLComputeUnits::All);
            configuration.setFunctionName(Some(&NSString::from_str(name)));
            let url = NSURL::fileURLWithPath(&NSString::from_str(&compiled.to_string_lossy()));
            objc2::exception::catch(AssertUnwindSafe(|| {
                MLModel::modelWithContentsOfURL_configuration_error(&url, &configuration)
            }))
            .map_err(|_| format!("exception loading {name}"))?
            .map_err(|error| format!("loading {name}: {}", nserr(&error)))
        })
    }

    fn plan_packs(lengths: &[usize]) -> (Vec<Vec<usize>>, Vec<usize>) {
        let mut bins = Vec::new();
        let mut singles = Vec::new();
        let mut current = Vec::new();
        let mut used = 0;
        for (index, &length) in lengths.iter().enumerate() {
            if length > PACK_LENGTH {
                singles.push(index);
                continue;
            }
            if used + length > PACK_LENGTH || current.len() == PACK_SLOTS {
                bins.push(std::mem::take(&mut current));
                used = 0;
            }
            current.push(index);
            used += length;
        }
        if !current.is_empty() {
            bins.push(current);
        }
        if bins.iter().all(|bin| bin.len() == 1) {
            singles.extend(bins.into_iter().flatten());
            singles.sort_unstable();
            (Vec::new(), singles)
        } else {
            (bins, singles)
        }
    }

    fn compiled_path(revision: &str) -> Result<PathBuf, String> {
        let cache = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache")))
            .ok_or_else(|| "HOME is not set".to_string())?;
        Ok(cache
            .join("rustxng")
            .join("embeddinggemma-2-coreml")
            .join(revision)
            .join("EmbeddingGemma2Text.mlmodelc"))
    }

    fn compile_into(package: &Path, destination: &Path) -> Result<(), String> {
        let parent = destination
            .parent()
            .ok_or_else(|| "compiled model has no parent directory".to_string())?;
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let materialized = parent.join(format!(
            "EmbeddingGemma2Text.compile.{}.mlpackage",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&materialized);
        copy_dir(package, &materialized)?;
        let result = autoreleasepool(|_| unsafe {
            let url = NSURL::fileURLWithPath(&NSString::from_str(&materialized.to_string_lossy()));
            let compiled = objc2::exception::catch(AssertUnwindSafe(|| {
                MLModel::compileModelAtURL_error(&url)
            }))
            .map_err(|_| format!("exception compiling {}", package.display()))?
            .map_err(|error| format!("compiling {}: {}", package.display(), nserr(&error)))?;
            compiled
                .path()
                .map(|path| PathBuf::from(path.to_string()))
                .ok_or_else(|| "compiled model URL has no path".to_string())
        });
        let _ = std::fs::remove_dir_all(materialized);
        let compiled = result?;
        let staging = destination.with_extension(format!("mlmodelc.tmp{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&staging);
        if std::fs::rename(&compiled, &staging).is_err() {
            copy_dir(&compiled, &staging)?;
            let _ = std::fs::remove_dir_all(&compiled);
        }
        match std::fs::rename(&staging, destination) {
            Ok(()) => Ok(()),
            Err(_) if destination.is_dir() => {
                let _ = std::fs::remove_dir_all(staging);
                Ok(())
            }
            Err(error) => Err(error.to_string()),
        }
    }

    fn copy_dir(source: &Path, destination: &Path) -> Result<(), String> {
        std::fs::create_dir_all(destination).map_err(|error| error.to_string())?;
        for entry in std::fs::read_dir(source).map_err(|error| error.to_string())? {
            let entry = entry.map_err(|error| error.to_string())?;
            let target = destination.join(entry.file_name());
            if entry
                .file_type()
                .map_err(|error| error.to_string())?
                .is_dir()
            {
                copy_dir(&entry.path(), &target)?;
            } else {
                std::fs::copy(entry.path(), target).map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    unsafe fn new_f16_array(
        dimensions: &[usize],
        data: &[f16],
    ) -> Result<Retained<MLMultiArray>, String> {
        let numbers: Vec<Retained<NSNumber>> = dimensions
            .iter()
            .map(|&dimension| NSNumber::new_isize(dimension as isize))
            .collect();
        let references: Vec<&NSNumber> = numbers.iter().map(|number| number.as_ref()).collect();
        let shape = NSArray::from_slice(&references);
        let array = MLMultiArray::initWithShape_dataType_error(
            MLMultiArray::alloc(),
            &shape,
            MLMultiArrayDataType::Float16,
        )
        .map_err(|error| format!("allocating Core ML input: {}", nserr(&error)))?;
        std::ptr::copy_nonoverlapping(
            data.as_ptr(),
            array.dataPointer().as_ptr() as *mut f16,
            data.len(),
        );
        Ok(array)
    }

    unsafe fn predict(
        model: &MLModel,
        features: &[(&str, &Retained<MLMultiArray>)],
    ) -> Result<Retained<MLMultiArray>, String> {
        let keys: Vec<Retained<NSString>> = features
            .iter()
            .map(|(name, _)| NSString::from_str(name))
            .collect();
        let values: Vec<Retained<MLFeatureValue>> = features
            .iter()
            .map(|(_, value)| MLFeatureValue::featureValueWithMultiArray(value))
            .collect();
        let key_refs: Vec<&NSString> = keys.iter().map(|key| key.as_ref()).collect();
        let value_refs: Vec<&MLFeatureValue> = values.iter().map(|value| value.as_ref()).collect();
        let dictionary: Retained<NSDictionary<NSString, MLFeatureValue>> =
            NSDictionary::from_slices(&key_refs, &value_refs);
        let provider = MLDictionaryFeatureProvider::initWithDictionary_error(
            MLDictionaryFeatureProvider::alloc(),
            std::mem::transmute::<
                &NSDictionary<NSString, MLFeatureValue>,
                &NSDictionary<NSString, AnyObject>,
            >(&dictionary),
        )
        .map_err(|error| format!("creating Core ML input: {}", nserr(&error)))?;
        let provider: &ProtocolObject<dyn MLFeatureProvider> = ProtocolObject::from_ref(&*provider);
        let result = objc2::exception::catch(AssertUnwindSafe(|| {
            model.predictionFromFeatures_error(provider)
        }))
        .map_err(|_| "exception during Core ML prediction".to_string())?
        .map_err(|error| format!("Core ML prediction: {}", nserr(&error)))?;
        result
            .featureValueForName(&NSString::from_str("embedding"))
            .ok_or_else(|| "Core ML output has no embedding".to_string())?
            .multiArrayValue()
            .ok_or_else(|| "Core ML embedding is not a multi-array".to_string())
    }

    unsafe fn read_embedding(array: &MLMultiArray, expected: usize) -> Result<Vec<f32>, String> {
        let values = read_output(array)?;
        if values.len() != expected {
            return Err(format!(
                "EmbeddingGemma 2 returned {} values, expected {expected}",
                values.len()
            ));
        }
        Ok(values)
    }

    unsafe fn read_output(array: &MLMultiArray) -> Result<Vec<f32>, String> {
        let dimensions = |numbers: Retained<NSArray<NSNumber>>| -> Result<Vec<usize>, String> {
            numbers
                .iter()
                .map(|number| {
                    usize::try_from(number.integerValue())
                        .map_err(|_| "negative Core ML shape or stride".to_string())
                })
                .collect()
        };
        let shape = dimensions(array.shape())?;
        let strides = dimensions(array.strides())?;
        let reach = shape
            .iter()
            .zip(&strides)
            .map(|(dimension, stride)| dimension.saturating_sub(1) * stride)
            .sum::<usize>()
            + 1;
        let pointer = array.dataPointer().as_ptr();
        if array.dataType() == MLMultiArrayDataType::Float32 {
            gather(
                std::slice::from_raw_parts(pointer as *const f32, reach),
                &shape,
                &strides,
            )
        } else if array.dataType() == MLMultiArrayDataType::Float16 {
            gather(
                std::slice::from_raw_parts(pointer as *const f16, reach),
                &shape,
                &strides,
            )
            .map(|values| values.into_iter().map(f16::to_f32).collect())
        } else {
            Err(format!(
                "unexpected Core ML output type {}",
                array.dataType().0
            ))
        }
    }

    fn gather<T: Copy>(data: &[T], shape: &[usize], strides: &[usize]) -> Result<Vec<T>, String> {
        if shape.len() != strides.len() {
            return Err("Core ML output shape and strides differ in rank".to_string());
        }
        let total = shape.iter().product();
        let mut output = Vec::with_capacity(total);
        let mut index = vec![0; shape.len()];
        for _ in 0..total {
            let offset: usize = index
                .iter()
                .zip(strides)
                .map(|(index, stride)| index * stride)
                .sum();
            output.push(data[offset]);
            for axis in (0..shape.len()).rev() {
                index[axis] += 1;
                if index[axis] < shape[axis] {
                    break;
                }
                index[axis] = 0;
            }
        }
        Ok(output)
    }

    fn nserr(error: &NSError) -> String {
        error.localizedDescription().to_string()
    }

    #[cfg(test)]
    mod tests {
        use super::plan_packs;

        #[test]
        fn packs_short_texts_and_keeps_long_texts_single() {
            assert_eq!(
                plan_packs(&[40, 40, 40, 40, 40, 40]),
                (vec![vec![0, 1, 2, 3, 4, 5]], vec![])
            );
            assert_eq!(plan_packs(&[300, 100, 100]), (vec![vec![1, 2]], vec![0]));
            assert_eq!(plan_packs(&[300, 100]), (vec![], vec![0, 1]));
        }
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    pub struct EmbeddingGemma2;

    impl EmbeddingGemma2 {
        pub fn load() -> Result<Self, String> {
            Err("EmbeddingGemma 2 semantic ranking requires macOS 15 or newer".into())
        }

        pub fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Err("EmbeddingGemma 2 semantic ranking requires macOS 15 or newer".into())
        }
    }
}

pub use platform::EmbeddingGemma2;
