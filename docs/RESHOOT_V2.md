# 补拍 v2 / Reshoot v2

开发与验收记录，2026-10-25。对应 [PR #69](https://github.com/ooolabdev/ooosplat/pull/69)，不代表所有平台已完成发布验收。

## 使用方式

从已完成项目创建补拍草稿。默认继承质量与自动优化，启动前可调整。视频沿用现有抽帧；超过100张补拍图片需回答顺序问题。全部确认后才创建后台任务。

| 原模型情况 | 初始方案 |
| --- | --- |
| 整体正常，只想补充细节 | 增量重建＋原模型微调 |
| 整体结构正常，但生成效果不满意 | 增量重建＋重新训练 |
| 明显变形或位置错乱 | 全量重建＋重新训练 |
| 不确定 | 增量重建＋重新训练 |

无法证明旧 PLY 与训练相机坐标一致时，摘要明确采用重新训练。外部导入模型不用于微调。所有工作在独立派生项目中进行，原素材、原项目、已发布 PLY 与编辑结果不改写。

## 重建和缓存

- 普通补拍允许跨分辨率、宽高比、设备和镜头。固定成像设置的视频按批次共享相机；普通照片保守使用独立 `SIMPLE_RADIAL` 相机，不凭设备名或相同尺寸合并内参。
- 原相机 ID 和已注册图片引用保留，最终稀疏模型的优化后内参仅同步到派生数据库。增量固定旧位姿和内参；全量不加载旧模型。局部/全局 BA 显式 Ceres。
- 小规模枚举新旧图片对；大规模用捆绑 COLMAP 的词袋检索，仅索引已注册旧图，每张新图取30个候选，零新增注册时扩大至60并重试增量一次。新增视频/已排序图片使用窗口10；其他图片穷举；闭环继承档位与自动优化规则。
- ERP 补拍保留原始球面 SfM；各批可有不同尺寸。仅注册成功的画面生成92°六面，面上限512/768/1024且不放大。RGB和Alpha同投影；点云保留3D初始化点，移除失效ERP观测与轨迹。
- 六面映射记录批次、原文件名、身份/方向、时间或自然顺序、ERP和派生相机；图片不伪造PTS。自动定位取最早有效来源的front面。
- 来源身份、问题答案、初始/实际方案、回退次数、实际训练参数和初始化哈希保存到可选检查点，不升级项目schema。旧任务缺少recipe时继续旧路径。恢复未完成训练是重新从该次初始化开始，不恢复优化器。
- 大型处理在阻塞工作线程执行；逐图/逐面转换和完整PLY校验支持取消，不并行驻留所有源图。

## 微调初始配方

| 参数 | 快速 | 均衡 | 精细 |
| --- | --- | --- | --- |
| 基础训练步数 | 3000 | 5000 | 8000 |
| 实际步数 | `min(完整配方步数, max(基础步数, 10×训练视图数))` | 同左 | 同左 |
| Refine | 200 | 200 | 200 |
| 梯度增密开始/截止 | 约15%/80%，对齐200步 | 同左 | 同左 |
| 增密阈值/选择比例 | 完整配方×1.5 / ×0.5 | 同左 | 同左 |

位置学习率初始/最终为Brush默认值×0.25；颜色、透明度、尺寸、旋转×0.5；位置噪声×0.1。保持原SH阶数和初始化点数，预算不得低于初始化点数；全景硬上限仍为1000万。完整重训不使用微调倍率。不冻结旧Gaussian，也不保证只改变补拍区域。

只有事前勾选授权，才允许零新增注册/旧坐标不兼容时全量重建一次，或明确数值发散/正常结束输出非法时冷启动一次。OOM、资源限制、进程崩溃、磁盘/文件错误、素材变化和取消不触发算法回退。低但非零注册只警告。原有有界OOM保护独立保留；不丢弃初始化点避开显存错误。各次训练保留独立尝试目录，恢复不重置回退次数。

## MCP

`inspect_reshoot_input` 只读分析已登记源项目及授权输入目录，返回源/输入身份。`create_generation_task` 可同时传入 `source_project_id` 和显式 `reshoot_options`（condition、allowFallback、sourceIdentity、inputIdentity、projection、可选imageMatchingStrategy），然后照常 `start_task`。不传新字段时旧协议不变；创建和执行前均重新校验身份。无明确授权不能扩大计算方案。

## 实测证据与限制

Windows x64，本机RTX 3060 Ti 8GB，捆绑COLMAP 4.2.1和现有OOOBrush；使用Ballintoy真实ERP图片与已有球面模型/原始生成PLY。

- 原项目56张已注册画面（1920×960），新增8张真实ERP图片（准备后2048×1024）。相机组不同，未将新图缩放至旧相机尺寸。
- 8/8新增画面注册成功，共64/64。旧模型209,467点；微调3000步后269,068点，输出63,501,665字节，总耗时225,753ms。原`final.ply`前后SHA-256一致。
- 首次隔离目录：`.cache/reshoot-v2-native/e2f76af79f0c4232ba5f4140fce0bea9/`。独立尝试目录和阻塞线程调整后复验：`.cache/reshoot-v2-native/9637c9a81e88446bb9406419451fba09/`，64/64注册、270,141点、63,754,893字节、229,207ms；源PLY哈希仍一致。两次均保存`events.json`、`result.json`、派生项目/引擎日志，不提交测试缓存。
- 使用隔离复制数据库真实执行`vocab_tree_retriever`：5张旧图、2张查询图、每张3候选，得到6对且全部属于指定旧图集合；约5秒。证明当前捆绑引擎支持分别索引旧图与查询新图，不以全库检索冒充30/60张旧候选。
- 自动检查：前端539项及诊断服务6项通过，前端构建通过；Rust 438项通过、20项手动/素材测试默认忽略。两个现有符号链接权限测试在本机返回Windows 1314，未修改其断言，单独过滤后完成其余Rust测试。Clippy `--all-targets -- -D warnings`通过。
- 第二次原生运行的资源采样：COLMAP进程观测内存最大160.5MiB，Brush为674.4MiB；显卡整卡已用显存观测最大3213MiB（含桌面和其他进程，不能当作本任务独占显存或真实峰值）。六面训练目录173,119,985字节。采样不包括应用进程转换图片的内存峰值。
- 测试直接调用生产执行核心，未登记到全局项目目录；不等同于已完成Tauri/桌面点击端到端验收。素材来自同一拍摄序列，不是新拍摄或真实跨设备/镜头验证。
- 未完成：真实跨设备普通图片/视频对照、不同相机镜头组合、连续多次原生补拍、全量及授权回退真实失败场景、桌面问答/队列操作、取消/磁盘故障原生验收、全景接缝和画质人工对照、macOS/Linux原生验收。
- 自动测试使用小型数据及隔离子进程覆盖参数、映射、状态和错误边界；不能据此声称所有硬件兼容或微调一定提升画质。训练过程尚未记录完整峰值显存/内存对照；资源采样不是峰值测量。

## English summary

Reshoot v2 preserves the source project and writes a derived project. Ordinary inputs may differ in size/aspect/device/lens; full monoscopic ERP inputs may differ across batches. Ordinary and ERP projections cannot be mixed. Conservative independent photo cameras avoid falsely sharing intrinsics. Incremental reconstruction fixes old poses/intrinsics; full reconstruction does not load the old sparse model. Both use explicit Ceres BA.

The four model-condition answers choose incremental+warm, incremental+cold, full+cold, and incremental+cold respectively. Slower algorithm fallbacks require a separate, unchecked-by-default authorization and are bounded across resumes. Resource/process/storage failures never authorize algorithm fallback. Warm-start uses the unedited generated PLY and a new optimizer; it is not optimizer-state continuation. Old and new views both train. Existing legacy tasks retain their saved recipe.

The Windows native Ballintoy core test registered all eight supplemental images (64 total), grew 209,467 to 269,068 splats in 3000 warm steps, and left the source PLY hash unchanged. It used real ERP photographs from the same capture, not real cross-device capture. Desktop interaction, broader failure/hardware coverage, macOS/Linux, visual seam/detail comparisons, and peak resource measurements remain unverified. See the Chinese sections above for parameters, local evidence paths, MCP request fields, and outstanding acceptance checks.
