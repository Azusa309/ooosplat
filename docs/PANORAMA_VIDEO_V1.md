# 全景视频首版 / Panorama video v1

开发分支 `feat/panorama-video-v1`；不是新版本发布声明。

## 输入和使用

仅支持 FFmpeg 可解码、已拼接、单目、完整 360°×180°、2:1 等距柱状（ERP）的 MP4。不按相机品牌设白名单；不支持双鱼眼、双目、VR180、裁切全景或厂商私有原片。请先使用厂商软件导出标准单目 ERP。

选择视频后检查首帧预览与“视频投影”。有完整球面元数据时自动识别；仅有 2:1 尺寸时，必须选择“普通视频”或“完整全景”。2:1 普通视频仍可走普通流程。已知不支持的投影不能选择全景。球面 yaw/pitch/roll 只是全局观察方向，不改变完整 ERP 的局部射线；SfM 使用自身世界坐标，不因品牌的观察方向元数据而拒绝。FFmpeg 的 180° 显示旋转仍是完整 ERP；90°/270° 会改变横纵布局，需先规范化导出。

CLI 示例（指定诊断目录时不会登记到应用列表，也不发送埋点）：

```powershell
cargo run --manifest-path src-tauri/Cargo.toml --bin splatstudio -- generate "D:/video/stitched-360.mp4" --projects-root "D:/diagnostic-projects" --quality fast --video-projection equirectangular
```

`--video-projection` 支持 `auto / perspective / equirectangular`。普通视频流程、已有项目检查点和普通补拍不变。全景项目首版暂不开放高清补拍。

## 实际处理流程

1. FFprobe 检查投影、立体布局、尺寸及旋转。全景抽帧另行流式读取解码帧索引、原始 PTS 和真实时间，不使用平均 FPS 推算时间戳；最多 100 万帧，时间探测输出只保留 4 KiB 诊断尾部。
2. 复用档位初始采样和 Bridge 候选预算，以原全景时刻为选择单位。保存整幅 ERP，而非六面图；Alpha 图和 Mask 保持现有语义。
3. COLMAP 模型 17 `EQUIRECTANGULAR`，参数为 **width,height**，不是焦距。顺序匹配与 Incremental Mapper；局部、全局 BA 显式 `CERES`，包括 Bridge 重建，完全绕过 Caspar。全景专用初始化三角化角度 1°，允许任意方向平移；普通透视参数不变。
4. 保留 `work/colmap/` 球面结果为权威重建。只有最终注册的 ERP 帧转换成 front/back/left/right/up/down 六面；不再匹配或重跑 SfM。
5. 派生 `work/panorama-training/`：六面共享中心，`R_face = Q R_erp`、`t_face = Q t_erp`。相机 x 向右、y 向下、z 向前，像素中心 `(x+0.5,y+0.5)`，主点 `(N/2,N/2)`，`f=N/(2 tan(46°))`。RGB 双线性采样、Mask 最近邻采样共享映射；ERP 左右取模环绕、两极钳制。
6. 六面共享 PINHOLE 相机。初始化三维点的位置、颜色、尺度保持不变；新图像 ID 从 1 编号，二维观测和旧 ERP 轨迹清空，避免悬空引用。
7. 沿用 Brush 的档位训练步数/增密日程，不乘六。结果继续发布 `final.ply`，预览、编辑和导出不变。注册率、缺失时段、Bridge 与效果统计仍按 ERP 帧计数。

### 默认资源预算

| 档位 | ERP SfM 最大宽度 | 每面最大边长 | 六面总像素上限 |
| --- | ---: | ---: | ---: |
| Fast | 2048 | 512 | 1.5 Mi pixels |
| Balanced | 3072 | 768 | 3.375 Mi pixels |
| High | 4096 | 1024 | 6 Mi pixels |

每面 FOV 92°，轻微重叠；面尺寸不超过实际 ERP 宽度的 1/4，不放大源图。逐时刻、逐面写盘，内存中仅保留单幅 ERP、可选 Mask 和一个面。单幅解码预算 512 MiB；抽帧与六面缓存分别做保守的 20 GiB 预算检查，超限明确失败，不静默减帧。Brush 图像缓存限 2 GiB，GPU 训练另需显存；这些预算不是整个进程/项目占用上限。

### 恢复与映射

- schema v9 增加可选全景检查点；旧项目仍按透视读取，不自动重算。
- 保存源视频 SHA-256、投影、转换版本、尺寸和 FOV。源文件或 SfM 尺寸变化使抽帧及下游检查点失效；仅面参数变化保留 SfM，重建训练缓存。投影类型变化要求新建任务。
- `work/panorama-frame-times.json` 保存选中及候选帧的源索引、PTS 和时间；`work/panorama-training/manifest.json` 将六面图关联到 ERP 图像、时间戳、相机姿态和配置，并保存球面模型指纹。
- 每个 ERP 的六面图作为完整批次原子发布。取消清理当前不完整批次，恢复时复用完整批次；模型文件在全部批次完成后生成，函数成功后才允许训练。
- 转换使用现有训练阶段进度/日志，显示已转换时刻；训练后显示 Brush 真实 step。训练中断仍按原行为重训，不宣称支持优化器状态续训。

## 验证记录（Windows，本地引擎）

`Ballintoy/Input` 是 94 幅真实 ERP JPEG，不含原始 MP4。只读原目录，在 `.cache/panorama-v1/` 合成 10 FPS、9.4 秒的测试 MP4；未复用素材自带 OpenVSLAM 位姿或过时相机参数。

- 打包 COLMAP 4.2.1：实际 CPU 球面特征提取、顺序匹配、Ceres 重建跑通，38/38 时刻注册。透视默认初始化门槛未得到模型，使用全景门槛后成功。
- 应用 Fast：56/56 ERP 时刻注册、7,987 个稀疏点、336 张 480×480 六面图、8,000 步、209,467 个 Gaussian、49,435,829 字节 `final.ply`；约 214 秒（debug，不作为性能承诺）。
- 真实取消/恢复已执行通过：转换两个完整批次后取消，恢复时复用抽帧、特征、匹配、球面重建和已完成六面批次；球面模型内容及修改时间不变。恢复训练产出 219,375 个 Gaussian，GPU 训练不要求位级复现。
- 用实际 `final.ply` 在 Edge WebGL2 渲染六个方向并对照训练图：前后、左右、上下均对应，无缺失半球；快速档的天空、海水和移动人物仍有模糊/拖影，不把几何对齐通过等同于画质完善。截图在 `.cache/panorama-v1/render-1792236975678/`。
- 自动测试覆盖前后半球、六面朝向、共中心、像素中心、Mask/接缝、初始化点保留、无失效轨迹、缓存恢复/损坏重建/取消、三档预算和不支持投影拒绝。真实引擎取消/恢复测试为显式 opt-in：

```powershell
node scripts/test-panorama-runtime.mjs "E:/GaussianSplatting/test/Ballintoy/Input"
$env:OOOSPLAT_PANORAMA_TEST_PROJECT="E:/path/to/completed/diagnostic-project"
cargo test --manifest-path src-tauri/Cargo.toml --lib bundled_panorama_cancel_and_resume -- --ignored --nocapture
node scripts/render-panorama-result.mjs "E:/path/to/completed/diagnostic-project"
```

还需实测：各品牌原生 MP4 元数据及 VFR/非零起始 PTS、原生透明全景视频、长视频磁盘/内存峰值、macOS/Linux 引擎、Balanced/High 最终画质与步数/方向覆盖调优。合成 MP4 跑通不能替代上述实测；不保证动态海水/人物、天空或低视差内容的重建质量。

## English summary

Development-only v1 supports **stitched, monoscopic, full 360×180, 2:1 ERP MP4**. A 2:1 ratio alone never enables panorama automatically: inspect the preview and explicitly choose ordinary video or full ERP when metadata is inconclusive. Stereo, dual-fisheye, VR180, cropped/private formats and quarter-turn display layouts are not silently interpreted as ERP. Static spherical viewing orientation is a global gauge, not a reason to reject standard ERP.

Whole ERP frames enter SfM with actual decoder indices/PTS/times, camera 17 (`width,height`), sequential matching and Incremental Mapper. Local/global BA explicitly use Ceres, including Bridge. Only registered frames become six co-centered 92° PINHOLE views, with correctly composed rotations **and translations**. RGB/masks share wrap-aware projection. Original sphere models remain authoritative; derived images have fresh IDs, empty observations and no stale ERP tracks; world-space initialization points remain unchanged.

Fast/Balanced/High: ERP width caps 2048/3072/4096, face caps 512/768/1024 without upsampling. Sequential conversion resumes in complete batches. Original Brush steps/densification remain; image cache is 2 GiB. No sixfold step multiplication, GPU projection or panorama reshoots. Draft choices survive restarts; older checkpoints remain perspective.

Windows acceptance used a 9.4-second MP4 synthesized from 94 actual Ballintoy ERP photographs, not supplied poses. CPU smoke: 38/38 registered. Full Fast: 56/56 registered, 7,987 points, 336 faces, 8,000 steps, 209,467 Gaussians, 49,435,829-byte PLY. Native-brand MP4/VFR metadata, native alpha video, long-resource stress, macOS/Linux engines, Balanced/High quality and direction tuning remain separate acceptance work. Unit/mock passes are not claims of visual quality.
