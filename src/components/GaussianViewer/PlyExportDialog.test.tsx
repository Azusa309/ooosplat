// @vitest-environment jsdom
import { act } from "react";
import { createRoot, type Root } from "react-dom/client";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LanguageProvider } from "../../i18n";
import { PlyExportDialog, type PlyExportDialogState } from "./PlyExportDialog";

describe("PlyExportDialog", () => {
  let root: Root;
  let container: HTMLDivElement;

  beforeEach(() => {
    (globalThis as typeof globalThis & { IS_REACT_ACT_ENVIRONMENT: boolean }).IS_REACT_ACT_ENVIRONMENT = true;
    localStorage.setItem("ooo-splat-language", "zh-CN");
    container = document.createElement("div");
    document.body.append(container);
    root = createRoot(container);
  });

  afterEach(async () => {
    await act(async () => root.unmount());
    container.remove();
    localStorage.clear();
  });

  const render = async (state: PlyExportDialogState, progress = 0, callbacks = {
    onClose: vi.fn(),
    onRetry: vi.fn(),
    onReveal: vi.fn(),
  }) => {
    await act(async () => root.render(<LanguageProvider>
      <PlyExportDialog state={state} progress={progress} {...callbacks} />
    </LanguageProvider>));
    return callbacks;
  };

  const button = (text: string) => Array.from(container.querySelectorAll("button")).find((item) => item.textContent === text)!;

  it("shows preparing and live export progress without a dismiss action", async () => {
    await render({ phase: "preparing" });
    expect(container.textContent).toContain("正在保存编辑并准备导出");
    expect(container.querySelectorAll("button")).toHaveLength(0);

    await render({ phase: "exporting" }, 47.9);
    expect(container.textContent).toContain("正在导出 PLY · 47%");
    expect(container.querySelector("progress")?.value).toBe(47.9);
    expect(container.querySelectorAll("button")).toHaveLength(0);
  });

  it("keeps a successful export open with reveal and close actions", async () => {
    const callbacks = await render({ phase: "completed", path: "E:\\Exports\\edit.ply" });
    expect(container.textContent).toContain("PLY 导出完成");
    expect(container.textContent).toContain("E:\\Exports\\edit.ply");

    await act(async () => button("打开文件夹").click());
    expect(callbacks.onReveal).toHaveBeenCalledWith("E:\\Exports\\edit.ply");
    await act(async () => button("关闭").click());
    expect(callbacks.onClose).toHaveBeenCalledOnce();
  });

  it("shows export errors and allows a retry", async () => {
    const callbacks = await render({ phase: "error", error: "disk full" });
    expect(container.textContent).toContain("PLY 导出失败：disk full");
    await act(async () => button("重试导出").click());
    expect(callbacks.onRetry).toHaveBeenCalledOnce();
  });

  it("ignores Escape while exporting and closes a completed result", async () => {
    const activeCallbacks = await render({ phase: "exporting" }, 10);
    await act(async () => container.querySelector("[role=dialog]")?.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(activeCallbacks.onClose).not.toHaveBeenCalled();

    const completedCallbacks = await render({ phase: "completed", path: "edit.ply" }, 100);
    await act(async () => container.querySelector("[role=dialog]")?.dispatchEvent(new KeyboardEvent("keydown", { key: "Escape", bubbles: true })));
    expect(completedCallbacks.onClose).toHaveBeenCalledOnce();
  });
});
