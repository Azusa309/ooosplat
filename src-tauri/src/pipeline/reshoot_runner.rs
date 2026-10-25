//! New reshoot execution is intentionally separate from the legacy runner.
use super::*;
use crate::pipeline::reshoot::*;
use crate::reconstruction::colmap_model::read_cameras;
use crate::video::panorama::{self, VideoProjection};
use sha2::{Digest, Sha256};

fn changed() -> SplatError {
    SplatError::Process("素材文件已发生变化，请重新分析后再生成".into())
}
fn invalid(detail: &str) -> SplatError {
    SplatError::Process(detail.into())
}

async fn validate_ply(
    path: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<(crate::reconstruction::ply::PlyInfo, u32, String)> {
    let path = path.to_owned();
    let cancel = cancel.clone();
    crate::presets::spawn_pipeline_blocking(move || {
        crate::reconstruction::splat_transform::validate_training_ply(&path, &cancel)
    })
    .await
    .map_err(|e| invalid(&format!("Gaussian validation worker: {e}")))?
}

struct SourceAssets {
    root: PathBuf,
    metadata: ProjectMetadata,
    state: PipelineStateFile,
    images: PathBuf,
    colmap_images: PathBuf,
    masks: PathBuf,
    database: PathBuf,
    model: PathBuf,
}

async fn source_assets(id: uuid::Uuid) -> Result<SourceAssets> {
    let (root, metadata) = catalog::load_registered_project(id).await?;
    if !catalog::project_is_durably_completed(&root, &metadata).await {
        return Err(invalid("只有已完成且结果完整的项目可以进行高清补拍"));
    }
    let state: PipelineStateFile =
        serde_json::from_slice(&tokio::fs::read(root.join("state.json")).await?)?;
    if let Some(imported) = &metadata.imported {
        let colmap = imported
            .colmap
            .as_ref()
            .ok_or_else(|| invalid("外部项目没有可复用的 COLMAP 数据"))?;
        crate::imports::revalidate_imported_colmap(colmap)?;
        let images = colmap
            .images_path
            .clone()
            .ok_or_else(|| invalid("外部 COLMAP 项目缺少原始图片"))?;
        return Ok(SourceAssets {
            images: images.clone(),
            colmap_images: images,
            masks: root.join("work/masks"),
            database: colmap
                .database_path
                .clone()
                .ok_or_else(|| invalid("外部 COLMAP 项目缺少 database.db"))?,
            model: colmap.sparse_model_path.clone(),
            root,
            metadata,
            state,
        });
    }
    let images = root.join("work/frames");
    let colmap_images = root.join("work/colmap_frames");
    let database = root.join("work/colmap/database.db");
    let masks = root.join("work/masks");
    let expected = state
        .frames
        .as_ref()
        .and_then(|f| f.extracted_frames)
        .ok_or_else(|| invalid("项目缺少已准备图片数量"))?;
    let names = crate::video::list_images(&images)?;
    if names.len() as u64 != expected
        || state.frames.as_ref().and_then(|f| f.colmap_input_version) != Some(COLMAP_INPUT_VERSION)
    {
        return Err(invalid(
            "原项目的 COLMAP 专用图片不完整或与原始帧不一致，无法安全进行增量补拍",
        ));
    }
    for image in names {
        let counterpart = colmap_images.join(
            image
                .file_name()
                .ok_or_else(|| invalid("Invalid image name"))?,
        );
        let header = crate::video::image_projection::inspect(&image)?;
        let counterpart_header = crate::video::image_projection::inspect(&counterpart)?;
        if (header.width, header.height) != (counterpart_header.width, counterpart_header.height) {
            return Err(invalid(
                "原项目缺少 COLMAP 专用图片，无法安全进行增量补拍。",
            ));
        }
    }
    let full = state
        .reshoot
        .as_ref()
        .and_then(|r| r.recipe.as_ref())
        .is_some_and(|r| r.actual.reconstruction == ReconstructionMode::Full);
    let model = if full {
        best_sparse_model(&images, &root.join("work/colmap/sparse-full"))
            .await?
            .0
    } else {
        reusable_reconstruction(&root, &images, metadata.output.as_ref())
            .await?
            .model
    };
    let cameras = read_cameras(&model)?;
    for image in read_registered_images(&model)? {
        let camera = cameras
            .iter()
            .find(|c| c.id == image.camera_id)
            .ok_or_else(|| invalid("Source image has no camera"))?;
        let header = crate::video::image_projection::inspect(&images.join(image.name))?;
        if (header.width as u64, header.height as u64) != (camera.width, camera.height) {
            return Err(invalid("Source image/camera dimensions differ"));
        }
    }
    let db = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .map_err(|e| invalid(&e.to_string()))?;
    for camera in &cameras {
        // BA legitimately refines intrinsics only in the sparse model. Verify
        // identity/geometry here; copy refined values into the DERIVED DB later.
        let row: (i32, u64, u64) = db
            .query_row(
                "SELECT model,width,height FROM cameras WHERE camera_id=?1",
                [camera.id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .map_err(|e| invalid(&e.to_string()))?;
        if row != (camera.model_id, camera.width, camera.height) {
            return Err(invalid("Source database/model camera identity differs"));
        }
    }
    if cameras
        .iter()
        .any(|c| (c.model_id == 17) != state.panorama.is_some())
    {
        return Err(invalid("Invalid source projection/camera mapping"));
    }
    Ok(SourceAssets {
        root,
        metadata,
        state,
        images,
        colmap_images,
        masks,
        database,
        model,
    })
}

fn source_identity(source: &SourceAssets) -> Result<String> {
    let mut hash = Sha256::new();
    for path in [
        source.root.join("project.json"),
        source.root.join("state.json"),
    ] {
        hash.update(std::fs::read(path)?);
    }
    let mut paths = reusable_model_files(&source.model)?;
    paths.push(source.database.clone());
    paths.push(
        catalog::project_model_path(&source.root, &source.metadata)
            .unwrap_or_else(|| source.root.join("final.ply")),
    );
    for path in paths {
        hash.update(path.to_string_lossy().as_bytes());
        let stat = path.metadata()?;
        hash.update(stat.len().to_le_bytes());
        hash.update(
            stat.modified()?
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| changed())?
                .as_nanos()
                .to_le_bytes(),
        );
    }
    // Cheap per-member identity, including already-prepared supplemental images.
    for dir in [&source.images, &source.colmap_images] {
        hash.update(panorama::source_identity(dir)?.as_bytes());
    }
    if source.masks.is_dir() {
        hash.update(panorama::source_identity(&source.masks)?.as_bytes());
    }
    Ok(format!("{:x}", hash.finalize()))
}

impl PipelineRunner {
    pub async fn validate_reshoot_request_v2(
        &self,
        project: uuid::Uuid,
        input: &Path,
        input_type: ReshootInputType,
        quality: Quality,
        options: &ReshootOptions,
    ) -> Result<()> {
        let source = self.inspect_reshoot_source_v2(project).await?;
        let probe = self
            .probe_reshoot_input_v2(project, input, input_type, quality)
            .await?;
        if source.source_identity != options.source_identity
            || probe.source_identity.as_deref() != Some(options.input_identity.as_str())
            || source.projection != options.projection
        {
            return Err(changed());
        }
        if !probe.compatible {
            return Err(invalid(
                probe
                    .incompatibility_reason
                    .as_deref()
                    .unwrap_or("Invalid input"),
            ));
        }
        let (root, metadata) = catalog::load_registered_project(project).await?;
        ensure_reshoot_input_is_new(&root, &metadata.source_path, input, input_type).await
    }

    pub async fn inspect_reshoot_source_v2(
        &self,
        project: uuid::Uuid,
    ) -> Result<ReshootSourceInfo> {
        let source = source_assets(project).await?;
        let cameras = read_cameras(&source.model)?;
        let first = cameras.first().ok_or_else(|| invalid("Missing cameras"))?;
        let model = catalog::project_model_path(&source.root, &source.metadata);
        let warm_available = source.metadata.imported.is_none()
            && model
                .as_ref()
                .is_some_and(|p| inspect_gaussian_ply(p).is_ok());
        Ok(ReshootSourceInfo {
            source_identity: source_identity(&source)?,
            projection: if source.state.panorama.is_some() {
                VideoProjection::Equirectangular
            } else {
                VideoProjection::Perspective
            },
            warm_available,
            project_id: project.to_string(),
            project_name: source.metadata.name,
            quality: source.metadata.quality,
            planner_enabled: source.state.planner_enabled,
            camera_id: first.id,
            camera_model: first.model.clone(),
            width: first.width,
            height: first.height,
            source_image_count: crate::video::list_images(&source.images)?.len() as u64,
            eligible: true,
            reason: None,
            imported: source.metadata.imported.is_some(),
        })
    }

    pub async fn probe_reshoot_input_v2(
        &self,
        project: uuid::Uuid,
        input: &Path,
        input_type: ReshootInputType,
        quality: Quality,
    ) -> Result<ReshootInputInfo> {
        let source = self.inspect_reshoot_source_v2(project).await?;
        let identity = panorama::source_identity(input)?;
        let samples = catalog::runtime_samples().await;
        let (count, duration, width, height, estimated, alpha, detection, sequence, estimate) =
            if input_type == ReshootInputType::Images {
                let scan = if source.projection == VideoProjection::Equirectangular {
                    crate::video::image_projection::analyze(input)?
                } else {
                    crate::video::image_projection::analyze_reshoot(input)?
                };
                let plan = crate::video::create_image_plan(&scan.info, &quality.preset());
                let estimate = estimate_runtime_for_images(
                    source.source_image_count + scan.info.image_count,
                    scan.info.width.max(scan.info.height),
                    &plan,
                    quality,
                    &samples,
                );
                (
                    Some(scan.info.image_count),
                    None,
                    scan.info.width,
                    scan.info.height,
                    scan.info.image_count,
                    scan.info.has_alpha,
                    scan.detection,
                    Some(scan.info),
                    estimate,
                )
            } else {
                let (video, json) = crate::engines::ffprobe::probe_video_metadata(
                    &self.engines.ffprobe,
                    input,
                    None,
                    &self.process_manager,
                )
                .await?;
                let mut detection = panorama::detect_metadata(
                    &json,
                    &video,
                    input
                        .extension()
                        .is_some_and(|s| s.eq_ignore_ascii_case("mp4")),
                );
                detection.source_identity = identity.clone();
                let plan = if self.planner_enabled {
                    QualityV2FrameSelection.create_plan(&video, &quality.preset())
                } else {
                    UniformRatioFrameSelection.create_plan(&video, &quality.preset())
                };
                let estimate = estimate_runtime(&video, &plan, quality, &samples);
                (
                    None,
                    Some(video.duration),
                    video.width,
                    video.height,
                    plan.estimated_frames,
                    video.has_alpha,
                    detection,
                    None,
                    estimate,
                )
            };
        let compatible = if source.projection == VideoProjection::Equirectangular {
            detection.supported_erp
        } else {
            !detection.confirmed_erp
        };
        Ok(ReshootInputInfo {
            source_identity: Some(identity),
            projection_detection: Some(detection),
            image_sequence: sequence,
            input_type,
            image_count: count,
            duration,
            prepared_width: width,
            prepared_height: height,
            estimated_frames: estimated,
            has_alpha: alpha,
            mask_count: if alpha { estimated } else { 0 },
            compatible,
            incompatibility_reason: (!compatible)
                .then(|| "全景项目仅支持完整全景视频或全景图片补拍，暂不支持普通素材。".into()),
            estimate,
        })
    }

    pub(super) async fn generate_reshoot_v2(
        &self,
        project: uuid::Uuid,
        input: &Path,
        input_type: ReshootInputType,
        root: &Path,
        quality: Quality,
        options: ReshootOptions,
    ) -> Result<PipelineResult> {
        let source = source_assets(project).await?;
        let info = self.inspect_reshoot_source_v2(project).await?;
        if options.source_identity != info.source_identity
            || options.input_identity != panorama::source_identity(input)?
            || options.projection != info.projection
        {
            return Err(changed());
        }
        let probe = self
            .probe_reshoot_input_v2(project, input, input_type, quality)
            .await?;
        if !probe.compatible {
            return Err(invalid(
                probe
                    .incompatibility_reason
                    .as_deref()
                    .unwrap_or("Invalid input"),
            ));
        }
        ensure_reshoot_input_is_new(
            &source.root,
            &source.metadata.source_path,
            input,
            input_type,
        )
        .await?;
        let acceleration = self.verify_pipeline_engines().await?;
        let manager =
            ProjectManager::with_root(root.into()).with_input_boundary(self.input_boundary.clone());
        let (paths, mut metadata) = manager
            .create_reshoot_v2(input, quality, self.process_manager.child_token())
            .await?;
        if panorama::source_identity(input)? != options.input_identity
            || source_identity(&source)? != options.source_identity
        {
            return Err(changed());
        }
        metadata.name = reshoot_project_name(&source.metadata.name);
        metadata.workspace_task_id = self.workspace_task_id;
        metadata.transform = source.metadata.transform;
        metadata.reshoot = Some(ReshootProvenance {
            source_project_id: project,
            source_project_path: source.root.clone(),
            source_final_ply: catalog::project_model_path(&source.root, &source.metadata)
                .unwrap_or_else(|| source.root.join("final.ply")),
            reshoot_source_path: metadata.source_path.clone(),
            camera_id: info.camera_id,
            camera_model: info.camera_model,
            width: info.width,
            height: info.height,
            has_alpha: probe.has_alpha,
            mask_count: 0,
            source_image_count: info.source_image_count,
            reshoot_frame_count: probe.estimated_frames,
            registered_reshoot_count: 0,
        });
        manager.write_metadata(&paths.metadata, &metadata).await?;
        let mut state = manager.read_state(&paths.state).await?;
        state.planner_enabled = self.planner_enabled;
        state.video_projection = options.projection;
        state.configuration = Some((*crate::presets::pipeline_optimization_config()).clone());
        let plan = ReshootPlan::resolve(options.condition, true, info.warm_available);
        state.reshoot = Some(ReshootState {
            recipe: Some(ReshootRecipe {
                version: RECIPE_VERSION,
                options,
                initial: plan,
                actual: plan,
                batch_id: paths.id.simple().to_string(),
                reconstruction_fallbacks: 0,
                training_fallbacks: 0,
                expanded_matching: false,
                source_names: vec![],
                new_names: vec![],
                warm_parameters: None,
                init_sha256: None,
                stored_input_identity: Some(panorama::source_identity(&metadata.source_path)?),
                panorama_frames: vec![],
            }),
            ..Default::default()
        });
        manager.write_state(&paths.state, &state).await?;
        self.execute_project(manager, paths, &mut metadata, state, &acceleration)
            .await
    }
}

fn normalize_image(
    source: &Path,
    frames: &Path,
    colmap: &Path,
    masks: &Path,
    name: &str,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<String> {
    use image::ImageDecoder;
    if cancel.is_cancelled() {
        return Err(SplatError::Cancelled);
    }
    let header = crate::video::image_projection::inspect(source)?;
    let name = if header.orientation != 1 {
        format!(
            "{}.png",
            Path::new(name).file_stem().unwrap().to_string_lossy()
        )
    } else {
        name.into()
    };
    let target = frames.join(&name);
    if header.alpha || header.orientation != 1 {
        let mut reader = image::ImageReader::open(source)?.with_guessed_format()?;
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(panorama::MAX_DECODE_BYTES);
        reader.limits(limits);
        let mut decoder = reader.into_decoder().map_err(|e| invalid(&e.to_string()))?;
        let orientation = decoder.orientation().map_err(|e| invalid(&e.to_string()))?;
        let mut decoded =
            image::DynamicImage::from_decoder(decoder).map_err(|e| invalid(&e.to_string()))?;
        decoded.apply_orientation(orientation);
        if header.orientation != 1 {
            decoded.save(&target).map_err(|e| invalid(&e.to_string()))?;
        } else {
            std::fs::copy(source, &target)?;
        }
        let rgba = decoded.to_rgba8();
        let mask = image::GrayImage::from_fn(rgba.width(), rgba.height(), |x, y| {
            image::Luma([if rgba.get_pixel(x, y)[3] >= 128 {
                255
            } else {
                0
            }])
        });
        mask.save(masks.join(format!("{name}.png")))
            .map_err(|e| invalid(&e.to_string()))?;
    } else {
        std::fs::copy(source, &target)?;
    }
    std::fs::copy(target, colmap.join(&name))?;
    Ok(name)
}

fn ensure_masks(
    images: &Path,
    masks: &Path,
    cancel: &tokio_util::sync::CancellationToken,
) -> Result<()> {
    for image in crate::video::list_images(images)? {
        if cancel.is_cancelled() {
            return Err(SplatError::Cancelled);
        }
        let mask = masks.join(format!(
            "{}.png",
            image.file_name().unwrap().to_string_lossy()
        ));
        let header = crate::video::image_projection::inspect(&image)?;
        if mask.exists() {
            if image::image_dimensions(&mask).map_err(|e| invalid(&e.to_string()))?
                != (header.width, header.height)
            {
                return Err(invalid("Mask/image dimensions differ"));
            }
        } else {
            if header.width as u64 * header.height as u64 > panorama::MAX_DECODE_BYTES {
                return Err(invalid("Image mask exceeds decode limit"));
            }
            image::GrayImage::from_pixel(header.width, header.height, image::Luma([255]))
                .save(mask)
                .map_err(|e| invalid(&e.to_string()))?;
        }
    }
    Ok(())
}

async fn save_recipe(
    manager: &ProjectManager,
    paths: &ProjectPaths,
    state: &mut PipelineStateFile,
    checkpoint: &ReshootState,
    recipe: &ReshootRecipe,
) -> Result<()> {
    let mut checkpoint = checkpoint.clone();
    checkpoint.recipe = Some(recipe.clone());
    state.stage =
        if !checkpoint.source_snapshot_complete || !checkpoint.supplemental_frames_complete {
            PipelineStage::ExtractingFrames
        } else if !checkpoint.supplemental_features_complete {
            PipelineStage::ExtractingFeatures
        } else if !checkpoint.incremental_matching_complete {
            PipelineStage::Matching
        } else if !checkpoint.incremental_reconstruction_complete {
            PipelineStage::Reconstructing
        } else {
            PipelineStage::TrainingSplats
        };
    state.reshoot = Some(checkpoint);
    manager.write_state(&paths.state, state).await
}

fn source_panorama_records(source: &SourceAssets) -> Result<Vec<PanoramaFrameRecord>> {
    if let Some(recipe) = source
        .state
        .reshoot
        .as_ref()
        .and_then(|s| s.recipe.as_ref())
    {
        return Ok(recipe.panorama_frames.clone());
    }
    let config = source
        .state
        .panorama
        .as_ref()
        .ok_or_else(|| invalid("Missing panorama source mapping"))?;
    if let Some(images) = &config.image_sources {
        return Ok(images
            .iter()
            .map(|i| PanoramaFrameRecord {
                prepared_name: i.prepared_name.clone(),
                batch_id: source.metadata.id.simple().to_string(),
                source_name: i.source_name.clone(),
                source_sha256: i.source_sha256.clone(),
                orientation: Some(i.orientation),
                source_index: i.source_image_index,
                timestamp_seconds: None,
                pts: None,
            })
            .collect());
    }
    let times: Vec<panorama::FrameTime> = serde_json::from_slice(&std::fs::read(
        source.root.join("work/panorama-frame-times.json"),
    )?)?;
    let mut records = Vec::new();
    for image in crate::video::list_images(&source.images)? {
        let stem = image
            .file_stem()
            .and_then(|v| v.to_str())
            .and_then(|v| v.strip_prefix("frame_"))
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or_else(|| invalid("Invalid panorama frame name"))?;
        let time = times
            .iter()
            .find(|t| t.source_frame_index == stem)
            .ok_or_else(|| invalid("Missing panorama timestamp mapping"))?;
        records.push(PanoramaFrameRecord {
            prepared_name: image.file_name().unwrap().to_string_lossy().into_owned(),
            batch_id: source.metadata.id.simple().to_string(),
            source_name: source
                .metadata
                .source_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            source_index: stem,
            source_sha256: config.source_sha256.clone(),
            orientation: None,
            timestamp_seconds: Some(time.timestamp_seconds),
            pts: Some(time.pts),
        });
    }
    Ok(records)
}

impl PipelineRunner {
    pub(super) async fn run_reshoot_v2(
        &self,
        manager: &ProjectManager,
        paths: &ProjectPaths,
        metadata: &mut ProjectMetadata,
        state: PipelineStateFile,
        acceleration: &crate::engines::ColmapAccelerationStatus,
    ) -> Result<PipelineResult> {
        let source_id = metadata
            .reshoot
            .as_ref()
            .ok_or_else(|| invalid("Missing reshoot provenance"))?
            .source_project_id;
        let source = source_assets(source_id).await?;
        self.execute_reshoot_v2(manager, paths, metadata, state, acceleration, source)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn execute_reshoot_v2(
        &self,
        manager: &ProjectManager,
        paths: &ProjectPaths,
        metadata: &mut ProjectMetadata,
        mut state: PipelineStateFile,
        acceleration: &crate::engines::ColmapAccelerationStatus,
        source: SourceAssets,
    ) -> Result<PipelineResult> {
        let mut checkpoint = state
            .reshoot
            .clone()
            .ok_or_else(|| invalid("Missing reshoot checkpoint"))?;
        let mut recipe = checkpoint
            .recipe
            .clone()
            .ok_or_else(|| invalid("Missing reshoot recipe"))?;
        if recipe.version != RECIPE_VERSION {
            return Err(invalid("Unsupported reshoot recipe"));
        }
        let provenance = metadata
            .reshoot
            .clone()
            .ok_or_else(|| invalid("Missing reshoot provenance"))?;
        if source_identity(&source)? != recipe.options.source_identity
            || recipe.stored_input_identity.as_deref()
                != Some(&panorama::source_identity(&metadata.source_path)?)
        {
            return Err(changed());
        }
        let is_panorama = recipe.options.projection == VideoProjection::Equirectangular;
        self.events
            .panorama_phase
            .store(u64::from(is_panorama), Ordering::Relaxed);
        self.events.panorama_images.store(
            metadata.input_type == ProjectInputType::Images,
            Ordering::Relaxed,
        );
        let cancel = self.process_manager.child_token();
        let database = paths.colmap.join("database.db");
        let base = paths.colmap.join("base-model");
        if !checkpoint.source_snapshot_complete {
            self.events.stage(
                PipelineStage::ExtractingFrames,
                0.,
                "正在复用原项目的相机与重建结果",
            );
            reset_directory(&paths.frames).await?;
            reset_directory(&paths.colmap_frames).await?;
            reset_directory(&paths.masks).await?;
            reset_directory(&paths.colmap).await?;
            copy_image_files(&source.images, &paths.frames).await?;
            copy_image_files(&source.colmap_images, &paths.colmap_frames).await?;
            if source.masks.is_dir() {
                copy_image_files(&source.masks, &paths.masks).await?;
            }
            tokio::fs::copy(&source.database, &database).await?;
            sync_refined_cameras(&database, &source.model)?;
            tokio::fs::create_dir_all(&base).await?;
            for file in reusable_model_files(&source.model)? {
                tokio::fs::copy(&file, base.join(file.file_name().unwrap())).await?;
            }
            recipe.source_names = crate::video::list_images(&paths.frames)?
                .into_iter()
                .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
                .collect();
            if is_panorama {
                recipe.panorama_frames = source_panorama_records(&source)?;
            }
            checkpoint.source_image_count = recipe.source_names.len() as u64;
            checkpoint.colmap_input_version = Some(COLMAP_INPUT_VERSION);
            checkpoint.source_snapshot_complete = true;
            save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
        }
        if !checkpoint.supplemental_frames_complete {
            self.events
                .stage(PipelineStage::ExtractingFrames, 0., "正在检查补拍素材");
            // Remove only this batch's uncommitted files after an interrupted preparation.
            let prefix = format!("reshoot_{}_", recipe.batch_id);
            for dir in [&paths.frames, &paths.colmap_frames, &paths.masks] {
                for entry in std::fs::read_dir(dir)? {
                    let p = entry?.path();
                    if p.file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(&prefix))
                        && p.is_file()
                    {
                        std::fs::remove_file(p)?;
                    }
                }
            }
            recipe
                .panorama_frames
                .retain(|f| f.batch_id != recipe.batch_id);
            let stage = paths.work.join("supplemental-preparation");
            reset_directory(&stage).await?;
            let stage_frames = stage.join("frames");
            let stage_colmap = stage.join("colmap");
            let stage_masks = stage.join("masks");
            for dir in [&stage_frames, &stage_colmap, &stage_masks] {
                tokio::fs::create_dir_all(dir).await?;
            }
            let mut batch_records = Vec::new();
            if metadata.input_type == ProjectInputType::Images && is_panorama {
                let config = crate::video::image_projection::checkpoint(
                    &metadata.source_path,
                    metadata.quality,
                    &cancel,
                )?;
                crate::video::image_projection::prepare(
                    &metadata.source_path,
                    &config,
                    &stage_frames,
                    &stage_colmap,
                    &stage_masks,
                    &cancel,
                    |current, total| {
                        self.events.send(
                            PipelineStage::ExtractingFrames,
                            Some(PipelineEngine::System),
                            EventKind::Progress,
                            EventLevel::Info,
                            Some(current as f32 / total.max(1) as f32),
                            false,
                            "正在准备全景图片",
                            Some(current),
                            Some(total),
                            Some("images"),
                        )
                    },
                )?;
                batch_records = config
                    .image_sources
                    .as_ref()
                    .unwrap()
                    .iter()
                    .map(|i| PanoramaFrameRecord {
                        prepared_name: i.prepared_name.clone(),
                        batch_id: recipe.batch_id.clone(),
                        source_name: i.source_name.clone(),
                        source_sha256: i.source_sha256.clone(),
                        orientation: Some(i.orientation),
                        source_index: i.source_image_index,
                        timestamp_seconds: None,
                        pts: None,
                    })
                    .collect();
                state.panorama = Some(config);
            } else if metadata.input_type == ProjectInputType::Images {
                let input = metadata.source_path.clone();
                let stage_frames = stage_frames.clone();
                let stage_colmap = stage_colmap.clone();
                let stage_masks = stage_masks.clone();
                let cancel = cancel.clone();
                crate::presets::spawn_pipeline_blocking(move || -> Result<()> {
                    for (index, image) in crate::video::list_images(&input)?.iter().enumerate() {
                        normalize_image(
                            image,
                            &stage_frames,
                            &stage_colmap,
                            &stage_masks,
                            &format!(
                                "frame_{index:010}.{}",
                                image.extension().unwrap().to_string_lossy()
                            ),
                            &cancel,
                        )?;
                    }
                    Ok(())
                })
                .await
                .map_err(|e| invalid(&format!("Image preparation worker: {e}")))??;
            } else {
                let video = probe_video(
                    &self.engines.ffprobe,
                    &metadata.source_path,
                    None,
                    &self.process_manager,
                )
                .await?;
                let config = if is_panorama {
                    Some(panorama::PanoramaCheckpoint::new_cancellable(
                        &metadata.source_path,
                        metadata.quality,
                        &video,
                        &cancel,
                    )?)
                } else {
                    None
                };
                let dimensions = config.as_ref().map(|c| (c.sfm_width, c.sfm_height));
                let prepared = self
                    .prepare_frames(
                        &metadata.source_path,
                        metadata.quality,
                        &stage_frames,
                        &stage_colmap,
                        &stage_masks,
                        Some(&paths.logs),
                        state.planner_enabled,
                        Some(video),
                        dimensions,
                        is_panorama,
                    )
                    .await?;
                state.video = prepared.video;
                state.frames = Some(FrameState::from(&prepared.plan));
                if let Some(config) = config {
                    let times: Vec<panorama::FrameTime> = serde_json::from_slice(&std::fs::read(
                        stage.join("panorama-frame-times.json"),
                    )?)?;
                    for image in crate::video::list_images(&stage_frames)? {
                        let index = image
                            .file_stem()
                            .unwrap()
                            .to_string_lossy()
                            .strip_prefix("frame_")
                            .and_then(|v| v.parse::<u64>().ok())
                            .ok_or_else(|| invalid("Invalid panorama source index"))?;
                        let t = times
                            .iter()
                            .find(|t| t.source_frame_index == index)
                            .ok_or_else(|| invalid("Missing panorama timestamp"))?;
                        batch_records.push(PanoramaFrameRecord {
                            prepared_name: image
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            batch_id: recipe.batch_id.clone(),
                            source_name: metadata
                                .source_path
                                .file_name()
                                .unwrap()
                                .to_string_lossy()
                                .into_owned(),
                            source_index: index,
                            source_sha256: config.source_sha256.clone(),
                            orientation: None,
                            timestamp_seconds: Some(t.timestamp_seconds),
                            pts: Some(t.pts),
                        });
                    }
                    state.panorama = Some(config);
                }
            }
            recipe.new_names.clear();
            for (index, image) in crate::video::list_images(&stage_frames)?
                .into_iter()
                .enumerate()
            {
                let old = image.file_name().unwrap().to_string_lossy().into_owned();
                let name = format!(
                    "{prefix}{index:010}.{}",
                    image.extension().unwrap().to_string_lossy()
                );
                for (from, to) in [
                    (&stage_frames, &paths.frames),
                    (&stage_colmap, &paths.colmap_frames),
                ] {
                    tokio::fs::copy(from.join(&old), to.join(&name)).await?;
                }
                let mask = stage_masks.join(format!("{old}.png"));
                if mask.is_file() {
                    tokio::fs::copy(mask, paths.masks.join(format!("{name}.png"))).await?;
                }
                if let Some(mut record) = batch_records
                    .iter()
                    .find(|r| r.prepared_name == old)
                    .cloned()
                {
                    record.prepared_name = name.clone();
                    recipe.panorama_frames.push(record);
                }
                recipe.new_names.push(name);
            }
            let images = paths.colmap_frames.clone();
            let masks = paths.masks.clone();
            let mask_cancel = cancel.clone();
            crate::presets::spawn_pipeline_blocking(move || {
                ensure_masks(&images, &masks, &mask_cancel)
            })
            .await
            .map_err(|e| invalid(&format!("Mask preparation worker: {e}")))??;
            checkpoint.reshoot_frame_count = recipe.new_names.len() as u64;
            checkpoint.supplemental_frames_complete = true;
            let total = checkpoint.source_image_count + checkpoint.reshoot_frame_count;
            state.frames = Some(FrameState {
                extracted_frames: Some(total),
                colmap_input_version: Some(COLMAP_INPUT_VERSION),
                estimated_frames: total,
                has_alpha: crate::video::list_images(&paths.frames)?.iter().any(|p| {
                    crate::video::image_projection::inspect(p).is_ok_and(|header| header.alpha)
                }),
                ..Default::default()
            });
            save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
        }
        let sfm = metadata.quality.preset();
        if !checkpoint.supplemental_features_complete {
            self.events.stage(
                PipelineStage::ExtractingFeatures,
                0.,
                "正在提取补拍画面的信息",
            );
            // Reset to the immutable base database: interrupted groups must not receive different IDs on resume.
            tokio::fs::copy(&source.database, &database).await?;
            sync_refined_cameras(&database, &base)?;
            let groups: Vec<Vec<String>> =
                if metadata.input_type == ProjectInputType::Video || is_panorama {
                    vec![recipe.new_names.clone()]
                } else {
                    recipe.new_names.iter().map(|n| vec![n.clone()]).collect()
                };
            for (index, group) in groups.iter().enumerate() {
                if cancel.is_cancelled() {
                    return Err(SplatError::Cancelled);
                }
                let list = paths.colmap.join(format!("group-{index}.txt"));
                write_image_list(&list, group).await?;
                let dims = if is_panorama {
                    let h = crate::video::image_projection::inspect(
                        &paths.colmap_frames.join(&group[0]),
                    )?;
                    Some((h.width, h.height))
                } else {
                    None
                };
                colmap::extract_reshoot_group(
                    &self.engines.colmap,
                    &database,
                    colmap::ReshootFeatureOptions {
                        images: &paths.colmap_frames,
                        list: &list,
                        masks: Some(&paths.masks),
                        panorama_dimensions: dims,
                        max_size: if state.planner_enabled {
                            sfm.planned_sfm_max_image_size
                        } else {
                            sfm.sfm_max_image_size
                        },
                        max_features: if state.planner_enabled {
                            sfm.planned_sfm_max_features
                        } else {
                            sfm.sfm_max_features
                        },
                    },
                    paths.logs.join("colmap.log"),
                    &self.process_manager,
                    Some(self.process_observer(
                        PipelineStage::ExtractingFeatures,
                        PipelineEngine::Colmap,
                        Some(group.len() as u64),
                        ObserverMode::BracketProgress,
                    )),
                    acceleration.gpu_index(),
                )
                .await?;
            }
            checkpoint.supplemental_features_complete = true;
            save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
        }
        self.reshoot_matching(
            paths,
            &state,
            &recipe,
            acceleration,
            recipe.expanded_matching,
        )
        .await?;
        checkpoint.incremental_matching_complete = true;
        save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
        if recipe.actual.reconstruction == ReconstructionMode::Full
            && !checkpoint.incremental_reconstruction_complete
        {
            self.reshoot_full_pairs(paths, &recipe, acceleration)
                .await?;
        }
        let model = loop {
            let output = paths.colmap.join(
                if recipe.actual.reconstruction == ReconstructionMode::Full {
                    "sparse-full"
                } else {
                    "sparse"
                },
            );
            if !checkpoint.incremental_reconstruction_complete {
                reset_directory(&output).await?;
                let mut names = recipe.source_names.clone();
                names.extend(recipe.new_names.clone());
                let list = paths.colmap.join("mapper-images.txt");
                write_image_list(&list, &names).await?;
                let fixed = paths.colmap.join("fixed-cameras.txt");
                let ids = read_cameras(&base)?
                    .into_iter()
                    .map(|c| c.id.to_string())
                    .collect::<Vec<_>>()
                    .join("\n");
                tokio::fs::write(&fixed, ids).await?;
                self.events.stage(
                    PipelineStage::Reconstructing,
                    0.,
                    "正在把补拍画面注册到原相机重建中",
                );
                colmap::map_reshoot(
                    &self.engines.colmap,
                    &database,
                    colmap::ReshootMapperOptions {
                        images: &paths.colmap_frames,
                        input: (recipe.actual.reconstruction == ReconstructionMode::Incremental)
                            .then_some(base.as_path()),
                        output: &output,
                        image_list: &list,
                        fixed_cameras: (recipe.actual.reconstruction
                            == ReconstructionMode::Incremental)
                            .then_some(fixed.as_path()),
                        panorama: is_panorama,
                    },
                    paths.logs.join("colmap.log"),
                    &self.process_manager,
                    Some(self.process_observer(
                        PipelineStage::Reconstructing,
                        PipelineEngine::Colmap,
                        Some(names.len() as u64),
                        ObserverMode::Mapper,
                    )),
                )
                .await?;
            }
            let (model, _) = best_sparse_model(&paths.frames, &output).await?;
            let poses = panorama::read_poses(&model)?;
            let added = poses
                .iter()
                .filter(|p| recipe.new_names.contains(&p.name))
                .count();
            let compatible = recipe.actual.reconstruction == ReconstructionMode::Full
                || coordinates_match(&base, &model)?;
            if added == 0
                && recipe.actual.reconstruction == ReconstructionMode::Incremental
                && !recipe.expanded_matching
            {
                recipe.expanded_matching = true;
                save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
                self.reshoot_matching(paths, &state, &recipe, acceleration, true)
                    .await?;
                checkpoint.incremental_reconstruction_complete = false;
                continue;
            }
            if added == 0 || !compatible {
                let reason = if added == 0 {
                    AlgorithmFailure::NoNewRegistrations
                } else {
                    AlgorithmFailure::CoordinateMismatch
                };
                if recipe.fallback(reason) {
                    checkpoint.incremental_reconstruction_complete = false;
                    state.brush_complete = false;
                    save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
                    self.events.stage(
                        PipelineStage::Reconstructing,
                        0.,
                        "当前方案未能完成，正在自动改用更耗时的方案。",
                    );
                    // Full reconstruction needs a connected graph including saved, unregistered images.
                    self.reshoot_full_pairs(paths, &recipe, acceleration)
                        .await?;
                    continue;
                }
                return Err(invalid(
                    "补拍画面没有成功加入原项目。请确保新旧素材有足够重叠。",
                ));
            }
            checkpoint.registered_reshoot_count = added as u64;
            checkpoint.incremental_reconstruction_complete = true;
            save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
            break model;
        };
        let report = ReconstructionValidator::validate(&paths.frames, &model)?;
        // The published sparse model contains refined new intrinsics. Record
        // them in this derived project's DB, never in the source project's DB.
        sync_refined_cameras(&database, &model)?;
        let dataset = if is_panorama {
            self.events.panorama_phase.store(2, Ordering::Relaxed);
            self.events
                .stage(PipelineStage::TrainingSplats, 0., "正在准备训练图片");
            let root = paths.work.join("panorama-training");
            let frames = paths.frames.clone();
            let model = model.clone();
            let records = recipe.panorama_frames.clone();
            let quality = metadata.quality;
            let cancel = cancel.clone();
            let events = self.events.clone();
            crate::presets::spawn_pipeline_blocking(move || {
                panorama::prepare_reshoot_training_cache(
                    &frames,
                    &model,
                    &root,
                    &records,
                    quality,
                    &cancel,
                    |update| match update {
                        panorama::TrainingCacheUpdate::Faces {
                            completed, total, ..
                        } => events.send(
                            PipelineStage::TrainingSplats,
                            Some(PipelineEngine::System),
                            EventKind::Progress,
                            EventLevel::Info,
                            Some(completed as f32 / total.max(1) as f32),
                            false,
                            "正在准备训练图片",
                            Some(completed),
                            Some(total),
                            Some("faces"),
                        ),
                        panorama::TrainingCacheUpdate::Validating => {
                            events.panorama_phase.store(3, Ordering::Relaxed);
                            events.stage(
                                PipelineStage::TrainingSplats,
                                0.,
                                "正在校验训练图片和相机数据",
                            );
                        }
                    },
                )
            })
            .await
            .map_err(|e| invalid(&format!("Panorama conversion worker: {e}")))??
        } else {
            let dataset = paths.brush.join("dataset");
            if !state.brush_complete {
                reset_directory(&dataset).await?;
                prepare_brush_dataset(&paths.brush, &paths.frames, &model).await?
            } else {
                dataset
            }
        };
        self.events
            .panorama_phase
            .store(u64::from(is_panorama), Ordering::Relaxed);
        let mut previous_training_failure = None;
        let candidate = loop {
            let mut attempt = paths.clone();
            let name = format!(
                "attempt-{}-{}",
                recipe.reconstruction_fallbacks, recipe.training_fallbacks
            );
            attempt.brush = paths.brush.join(&name);
            attempt.logs = paths.logs.join(&name);
            tokio::fs::create_dir_all(&attempt.logs).await?;
            if state.brush_complete {
                if let Some(candidate) = brush_candidate(&attempt.brush) {
                    validate_ply(&candidate, &cancel).await?;
                    break candidate;
                }
                state.brush_complete = false;
            }
            let policy = brush::BrushGpuLaunchPolicy::from_acceleration(acceleration);
            let max_edge = recipe
                .source_names
                .iter()
                .chain(&recipe.new_names)
                .filter_map(|n| crate::video::image_projection::inspect(&paths.frames.join(n)).ok())
                .map(|i| i.width.max(i.height))
                .max()
                .unwrap_or(1);
            let face_size = match metadata.quality {
                Quality::Fast => 512,
                Quality::Balanced => 768,
                Quality::High => 1024,
            };
            let mut resolved = state.brush_training.resolved.unwrap_or(if is_panorama {
                resolve_panorama_training_preset(
                    metadata.quality,
                    state.planner_enabled,
                    policy.known_training_memory_mb(acceleration),
                    face_size,
                    report.points_3d,
                )?
            } else {
                resolve_brush_training_preset(
                    metadata.quality,
                    state.planner_enabled,
                    acceleration.usable_gpu_total_memory_mb(),
                    max_edge,
                    report.points_3d,
                )
            });
            let mut initial_points = report.points_3d;
            if recipe.actual.training == TrainingMode::Warm {
                let init = dataset.join("init.ply");
                let model = provenance.source_final_ply.clone();
                let (_, _, original_hash) = validate_ply(&model, &cancel).await?;
                if !init.is_file() || recipe.init_sha256.as_deref() != Some(&original_hash) {
                    let pending = dataset.join("init.ply.partial");
                    tokio::fs::copy(&model, &pending).await?;
                    let (_, _, snapshot_hash) = validate_ply(&pending, &cancel).await?;
                    if snapshot_hash != original_hash {
                        return Err(changed());
                    }
                    atomic_replace_file(&pending, &init).await?;
                    recipe.init_sha256 = Some(original_hash);
                }
                let (info, sh, _) = validate_ply(&init, &cancel).await?;
                initial_points = info.splat_count;
                if is_panorama && initial_points > crate::presets::MAX_PANORAMA_SPLATS as u64 {
                    return Err(invalid(
                        "初始化点数超出当前全景训练预算，已停止训练以避免丢弃初始点。",
                    ));
                }
                let views = read_registered_images(&dataset.join("sparse/0"))?.len() as u64;
                if recipe.warm_parameters.is_none() {
                    let (preset, warm) = warm_training(
                        resolved.preset,
                        metadata.quality,
                        views,
                        initial_points,
                        sh,
                    )?;
                    resolved.preset = preset;
                    recipe.warm_parameters = Some(warm);
                }
                if let Some(snapshot) = resolved.panorama.as_mut() {
                    snapshot.initial_points = initial_points;
                    snapshot.initial_points_override = resolved
                        .configured_max_splats
                        .is_some_and(|cap| initial_points > cap as u64)
                        || snapshot
                            .memory_max_splats
                            .is_some_and(|cap| initial_points > cap as u64);
                }
            } else {
                let init = dataset.join("init.ply");
                if init.is_file() {
                    tokio::fs::rename(init, dataset.join("init.previous")).await?;
                }
                recipe.warm_parameters = None;
            }
            state.brush_training.resolved = Some(resolved);
            save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
            self.events
                .stage(PipelineStage::TrainingSplats, 0., "正在生成高斯泼溅");
            let outcome = self
                .train_brush_with_oom_retry(
                    manager,
                    &attempt,
                    &mut state,
                    metadata.quality,
                    acceleration,
                    &dataset,
                    initial_points,
                    resolved,
                    0,
                )
                .await;
            let candidate = match outcome {
                Ok(path) => path,
                Err(error)
                    if explicit_numeric_divergence(&error)
                        && recipe.fallback(AlgorithmFailure::InvalidWarmResult) =>
                {
                    previous_training_failure = Some(error);
                    state.brush_training.resolved = None;
                    save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
                    self.events.stage(
                        PipelineStage::TrainingSplats,
                        0.,
                        "当前方案未能完成，正在自动改用更耗时的方案。",
                    );
                    continue;
                }
                Err(error) => {
                    return Err(if let Some(previous) = previous_training_failure.take() {
                        error.with_previous_attempt(previous)
                    } else {
                        error
                    })
                }
            };
            match validate_ply(&candidate, &cancel).await {
                Ok(_) => {
                    state.brush_complete = true;
                    save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
                    break candidate;
                }
                Err(error) => {
                    if !matches!(error, SplatError::Io(_) | SplatError::Cancelled)
                        && recipe.fallback(AlgorithmFailure::InvalidWarmResult)
                    {
                        state.brush_training.resolved = None;
                        save_recipe(manager, paths, &mut state, &checkpoint, &recipe).await?;
                        self.events.stage(
                            PipelineStage::TrainingSplats,
                            0.,
                            "当前方案未能完成，正在自动改用更耗时的方案。",
                        );
                    } else {
                        return Err(error);
                    }
                }
            }
        };
        self.publish_reshoot_v2(
            manager,
            paths,
            metadata,
            &mut state,
            &checkpoint,
            &report,
            &candidate,
        )
        .await
    }

    async fn reshoot_matching(
        &self,
        paths: &ProjectPaths,
        state: &PipelineStateFile,
        recipe: &ReshootRecipe,
        acceleration: &crate::engines::ColmapAccelerationStatus,
        expanded: bool,
    ) -> Result<()> {
        if state
            .reshoot
            .as_ref()
            .is_some_and(|s| s.incremental_matching_complete)
            && !expanded
        {
            return Ok(());
        }
        self.events
            .stage(PipelineStage::Matching, 0., "正在补充新旧画面之间的联系");
        let database = paths.colmap.join("database.db");
        let list = paths.colmap.join("reshoot-images.txt");
        write_image_list(&list, &recipe.new_names).await?;
        use std::io::Write;
        let pair_list = paths.colmap.join("reshoot-pairs.txt");
        let mut pairs = std::io::BufWriter::new(std::fs::File::create(&pair_list)?);
        if recipe
            .source_names
            .len()
            .saturating_mul(recipe.new_names.len())
            <= 20_000
        {
            for old in &recipe.source_names {
                for new in &recipe.new_names {
                    writeln!(pairs, "{old} {new}")?;
                }
            }
        } else {
            let registered = read_registered_images(&paths.colmap.join("base-model"))?;
            let old: Vec<_> = recipe
                .source_names
                .iter()
                .filter(|n| registered.iter().any(|image| image.name == **n))
                .cloned()
                .collect();
            for (old, new) in colmap::retrieve_reshoot_pairs(
                &self.engines.colmap,
                &database,
                &old,
                &recipe.new_names,
                if expanded { 60 } else { 30 },
                paths.logs.join("colmap.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    None,
                    ObserverMode::Matching,
                )),
            )
            .await?
            {
                writeln!(pairs, "{old} {new}")?;
            }
        }
        let sequential = state.input_type == ProjectInputType::Video
            || (recipe.new_names.len() > 100
                && recipe.options.image_matching_strategy
                    == crate::project::ImageMatchingStrategy::Sequential);
        for (i, a) in recipe.new_names.iter().enumerate() {
            if self.process_manager.child_token().is_cancelled() {
                return Err(SplatError::Cancelled);
            }
            for b in
                recipe
                    .new_names
                    .iter()
                    .skip(i + 1)
                    .take(if sequential { 10 } else { usize::MAX })
            {
                writeln!(pairs, "{a} {b}")?;
            }
        }
        pairs.flush()?;
        drop(pairs);
        if std::fs::metadata(&pair_list)?.len() > 0 {
            colmap::match_pairs(
                &self.engines.colmap,
                &database,
                &pair_list,
                paths.logs.join("colmap.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    None,
                    ObserverMode::Matching,
                )),
                acceleration.gpu_index(),
            )
            .await?;
        }
        if sequential && state.preset.loop_detection_enabled(state.planner_enabled) {
            colmap::match_reshoot_candidates(
                &self.engines.colmap,
                &database,
                &list,
                50,
                paths.logs.join("colmap.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    None,
                    ObserverMode::Matching,
                )),
                acceleration.gpu_index(),
            )
            .await?;
        }
        Ok(())
    }

    async fn reshoot_full_pairs(
        &self,
        paths: &ProjectPaths,
        recipe: &ReshootRecipe,
        acceleration: &crate::engines::ColmapAccelerationStatus,
    ) -> Result<()> {
        let all: Vec<_> = recipe
            .source_names
            .iter()
            .chain(&recipe.new_names)
            .cloned()
            .collect();
        let database = paths.colmap.join("database.db");
        if all.len() <= 200 {
            colmap::match_exhaustive(
                &self.engines.colmap,
                &database,
                paths.logs.join("colmap.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    None,
                    ObserverMode::Matching,
                )),
                acceleration.gpu_index(),
            )
            .await?;
        } else {
            let list = paths.colmap.join("all-images.txt");
            write_image_list(&list, &all).await?;
            colmap::match_reshoot_candidates(
                &self.engines.colmap,
                &database,
                &list,
                60,
                paths.logs.join("colmap.log"),
                &self.process_manager,
                Some(self.process_observer(
                    PipelineStage::Matching,
                    PipelineEngine::Colmap,
                    None,
                    ObserverMode::Matching,
                )),
                acceleration.gpu_index(),
            )
            .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn publish_reshoot_v2(
        &self,
        manager: &ProjectManager,
        paths: &ProjectPaths,
        metadata: &mut ProjectMetadata,
        state: &mut PipelineStateFile,
        checkpoint: &ReshootState,
        report: &ReconstructionReport,
        candidate: &Path,
    ) -> Result<PipelineResult> {
        let ply = inspect_gaussian_ply(candidate)?;
        let final_ply = paths.project.join("final.ply");
        atomic_replace_file(candidate, &final_ply).await?;
        let completed_at = Utc::now();
        let duration_ms = metadata.duration_ms.unwrap_or(0).saturating_add(
            metadata
                .started_at
                .map(|t| (completed_at - t).num_milliseconds().max(0) as u64)
                .unwrap_or(0),
        );
        if let Some(provenance) = metadata.reshoot.as_mut() {
            provenance.registered_reshoot_count = checkpoint.registered_reshoot_count;
            provenance.reshoot_frame_count = checkpoint.reshoot_frame_count;
        }
        metadata.status = ProjectStatus::Completed;
        metadata.completed_at = Some(completed_at);
        metadata.duration_ms = Some(duration_ms);
        metadata.output = Some(ProjectOutput {
            final_ply: final_ply.clone(),
            file_size: ply.file_size,
            splat_count: ply.splat_count,
            input_images: report.input_images,
            registered_images: report.registered_images,
            registered_ratio: report.registered_ratio,
            points_3d: report.points_3d,
        });
        state.stage = PipelineStage::Completed;
        manager.write_state(&paths.state, state).await?;
        manager.write_metadata(&paths.metadata, metadata).await?;
        let warning = (checkpoint.registered_reshoot_count as f64
            / (checkpoint.reshoot_frame_count.max(1) as f64)
            < crate::reconstruction::good_registered_ratio())
        .then(|| {
            format!(
                "补拍画面成功加入 {}/{}，结果已生成，但部分补拍画面未能找到与原项目的联系。",
                checkpoint.registered_reshoot_count, checkpoint.reshoot_frame_count
            )
        });
        self.events
            .stage(PipelineStage::Completed, 1., "高清补拍处理完成");
        Ok(PipelineResult {
            project_id: paths.id.to_string(),
            project_path: paths.project.clone(),
            final_ply,
            file_size: ply.file_size,
            splat_count: ply.splat_count,
            input_images: report.input_images,
            registered_images: report.registered_images,
            registered_ratio: report.registered_ratio,
            points_3d: report.points_3d,
            duration_ms,
            completed_at,
            warning,
            logs_directory: paths.logs.clone(),
            source_duration_seconds: None,
        })
    }
}

fn coordinates_match(base: &Path, updated: &Path) -> Result<bool> {
    let before = read_cameras(base)?;
    let after = read_cameras(updated)?;
    for camera in before {
        if !after.iter().any(|c| c.id == camera.id && c == &camera) {
            return Ok(false);
        }
    }
    let old = panorama::read_poses(base)?;
    let new = panorama::read_poses(updated)?;
    for pose in old {
        let Some(other) = new
            .iter()
            .find(|p| p.name == pose.name && p.camera_id == pose.camera_id)
        else {
            return Ok(false);
        };
        let dot: f64 = pose.qvec.iter().zip(other.qvec).map(|(a, b)| a * b).sum();
        if (1. - dot.abs()).abs() > 1e-10
            || pose
                .tvec
                .iter()
                .zip(other.tvec)
                .any(|(a, b)| (a - b).abs() > 1e-8 * (1. + a.abs()))
        {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sync_refined_cameras(database: &Path, model: &Path) -> Result<()> {
    let connection = rusqlite::Connection::open(database).map_err(|e| invalid(&e.to_string()))?;
    for camera in read_cameras(model)? {
        let params: Vec<_> = camera.params.iter().flat_map(|v| v.to_le_bytes()).collect();
        connection
            .execute(
                "UPDATE cameras SET params=?1 WHERE camera_id=?2",
                rusqlite::params![params, camera.id],
            )
            .map_err(|e| invalid(&e.to_string()))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mixed_reshoot_images_leave_regular_generation_strict_and_preserve_sources() {
        let root = tempfile::tempdir().unwrap();
        let input = root.path().join("input");
        std::fs::create_dir(&input).unwrap();
        for (name, w, h) in [("相机2.png", 12, 8), ("相机10.png", 8, 12)] {
            image::RgbaImage::from_pixel(w, h, image::Rgba([120, 40, 80, 128]))
                .save(input.join(name))
                .unwrap();
        }
        assert!(crate::video::image_projection::analyze(&input).is_err());
        let scan = crate::video::image_projection::analyze_reshoot(&input).unwrap();
        assert_eq!(scan.info.image_count, 2);
        assert_eq!(scan.images[0].path.file_name().unwrap(), "相机2.png");
        let frames = root.path().join("frames");
        let colmap = root.path().join("colmap");
        let masks = root.path().join("masks");
        for dir in [&frames, &colmap, &masks] {
            std::fs::create_dir(dir).unwrap();
        }
        let before = std::fs::read(&scan.images[0].path).unwrap();
        let token = tokio_util::sync::CancellationToken::new();
        for (i, item) in scan.images.iter().enumerate() {
            normalize_image(
                &item.path,
                &frames,
                &colmap,
                &masks,
                &format!("reshoot_batch_{i}.png"),
                &token,
            )
            .unwrap();
        }
        ensure_masks(&colmap, &masks, &token).unwrap();
        assert_eq!(std::fs::read(&scan.images[0].path).unwrap(), before);
        assert_eq!(
            image::image_dimensions(masks.join("reshoot_batch_1.png.png")).unwrap(),
            (8, 12)
        );
        assert_eq!(
            image::open(frames.join("reshoot_batch_0.png"))
                .unwrap()
                .to_rgba8()
                .get_pixel(0, 0)[3],
            128
        );
        token.cancel();
        assert!(matches!(
            ensure_masks(&colmap, &masks, &token),
            Err(SplatError::Cancelled)
        ));
    }

    #[test]
    fn independent_camera_groups_and_pose_signs_preserve_the_same_coordinate_frame() {
        let root = tempfile::tempdir().unwrap();
        for dir in ["before", "after"] {
            let path = root.path().join(dir);
            std::fs::create_dir(&path).unwrap();
            std::fs::write(
                path.join("cameras.txt"),
                "1 SIMPLE_RADIAL 20 10 15 10 5 0\n3 SIMPLE_RADIAL 12 16 10 6 8 0\n",
            )
            .unwrap();
            std::fs::write(path.join("images.txt"), "1 1 0 0 0 1 2 3 1 old.jpg\n\n").unwrap();
        }
        assert!(
            coordinates_match(&root.path().join("before"), &root.path().join("after")).unwrap()
        );
        std::fs::write(
            root.path().join("after/images.txt"),
            "1 -1 0 0 0 1 2 3 1 old.jpg\n\n",
        )
        .unwrap();
        assert!(
            coordinates_match(&root.path().join("before"), &root.path().join("after")).unwrap()
        );
        std::fs::write(
            root.path().join("after/images.txt"),
            "1 1 0 0 0 5 2 3 1 old.jpg\n\n",
        )
        .unwrap();
        assert!(
            !coordinates_match(&root.path().join("before"), &root.path().join("after")).unwrap()
        );
    }

    #[tokio::test]
    #[ignore = "Requires the local real Ballintoy source project, bundled engines, and GPU; writes only an isolated .cache project"]
    async fn native_ballintoy_reshoot_v2() {
        let repo = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let original = repo.join(".cache/panorama-v1/projects/20261017-191833_ballintoy-erp");
        let source_metadata: ProjectMetadata =
            serde_json::from_slice(&std::fs::read(original.join("project.json")).unwrap()).unwrap();
        let source_state: PipelineStateFile =
            serde_json::from_slice(&std::fs::read(original.join("state.json")).unwrap()).unwrap();
        let model = best_sparse_model(
            &original.join("work/frames"),
            &original.join("work/colmap/sparse"),
        )
        .await
        .unwrap()
        .0;
        let original_hash = panorama::fingerprint(&original.join("final.ply")).unwrap();
        let source = SourceAssets {
            root: original.clone(),
            metadata: source_metadata.clone(),
            state: source_state,
            images: original.join("work/frames"),
            colmap_images: original.join("work/colmap_frames"),
            masks: original.join("work/masks"),
            database: original.join("work/colmap/database.db"),
            model,
        };
        let identity = source_identity(&source).unwrap();
        let test_root = repo.join(".cache/reshoot-v2-native");
        std::fs::create_dir_all(&test_root).unwrap();
        let run = test_root.join(uuid::Uuid::new_v4().simple().to_string());
        std::fs::create_dir(&run).unwrap();
        let input = run.join("new-input");
        std::fs::create_dir(&input).unwrap();
        let originals =
            crate::video::list_images(Path::new("E:/GaussianSplatting/test/Ballintoy/Input"))
                .unwrap();
        for i in [0, 5, 10, 15, 20, 25, 30, 35] {
            std::fs::copy(&originals[i], input.join(originals[i].file_name().unwrap())).unwrap();
        }
        let manager = ProjectManager::for_diagnostics(run.join("projects"));
        let (paths, mut metadata) = manager
            .create_reshoot_v2(
                &input,
                Quality::Fast,
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .unwrap();
        metadata.reshoot = Some(ReshootProvenance {
            source_project_id: source_metadata.id,
            source_project_path: original.clone(),
            source_final_ply: original.join("final.ply"),
            reshoot_source_path: metadata.source_path.clone(),
            camera_id: 1,
            camera_model: "EQUIRECTANGULAR".into(),
            width: 1920,
            height: 960,
            has_alpha: false,
            mask_count: 0,
            source_image_count: 56,
            reshoot_frame_count: 8,
            registered_reshoot_count: 0,
        });
        let mut state = manager.read_state(&paths.state).await.unwrap();
        state.video_projection = VideoProjection::Equirectangular;
        state.planner_enabled = true;
        let options = ReshootOptions {
            condition: ModelCondition::Good,
            allow_fallback: false,
            source_identity: identity,
            input_identity: panorama::source_identity(&input).unwrap(),
            projection: VideoProjection::Equirectangular,
            image_matching_strategy: crate::project::ImageMatchingStrategy::Exhaustive,
        };
        let plan = ReshootPlan::resolve(ModelCondition::Good, true, true);
        state.reshoot = Some(ReshootState {
            recipe: Some(ReshootRecipe {
                version: RECIPE_VERSION,
                options,
                initial: plan,
                actual: plan,
                batch_id: paths.id.simple().to_string(),
                reconstruction_fallbacks: 0,
                training_fallbacks: 0,
                expanded_matching: false,
                source_names: vec![],
                new_names: vec![],
                warm_parameters: None,
                init_sha256: None,
                stored_input_identity: Some(
                    panorama::source_identity(&metadata.source_path).unwrap(),
                ),
                panorama_frames: vec![],
            }),
            ..Default::default()
        });
        manager.write_state(&paths.state, &state).await.unwrap();
        manager
            .write_metadata(&paths.metadata, &metadata)
            .await
            .unwrap();
        let events = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = events.clone();
        let runner = PipelineRunner::new_with_planner(
            EnginePaths::from_root(repo.join("engines")),
            true,
            move |event| {
                log.lock().unwrap().push(event);
            },
        );
        let acceleration = runner.verify_pipeline_engines().await.unwrap();
        eprintln!("Real reshoot project: {}", paths.project.display());
        let result = runner
            .execute_reshoot_v2(
                &manager,
                &paths,
                &mut metadata,
                state,
                &acceleration,
                source,
            )
            .await;
        std::fs::write(
            run.join("events.json"),
            serde_json::to_vec_pretty(&*events.lock().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            panorama::fingerprint(&original.join("final.ply")).unwrap(),
            original_hash
        );
        let result = result.unwrap();
        std::fs::write(
            run.join("result.json"),
            serde_json::to_vec_pretty(&result).unwrap(),
        )
        .unwrap();
        let restored = manager.read_state(&paths.state).await.unwrap();
        assert!(restored.reshoot.as_ref().unwrap().registered_reshoot_count > 0);
        assert_eq!(
            restored.reshoot.unwrap().recipe.unwrap().actual.training,
            TrainingMode::Warm
        );
        assert!(result.splat_count > 0);
    }
}
