import { Download, FolderOpen, LoaderCircle, X } from "lucide-react";
import { useI18n } from "../../i18n";

export type PlyExportDialogState =
  | { phase: "closed" }
  | { phase: "preparing" }
  | { phase: "exporting" }
  | { phase: "completed"; path: string; revealError?: string }
  | { phase: "error"; error: string };

interface Props {
  state: PlyExportDialogState;
  progress: number;
  onClose: () => void;
  onRetry: () => void;
  onReveal: (path: string) => void;
}

export function PlyExportDialog({ state, progress, onClose, onRetry, onReveal }: Props) {
  const { t } = useI18n();
  if (state.phase === "closed") return null;
  const active = state.phase === "preparing" || state.phase === "exporting";
  const title = state.phase === "completed"
    ? t("export.plyCompleted")
    : state.phase === "error"
      ? t("export.plyFailedTitle")
      : t("export.ply");

  return <div
    className="gaussian-save-backdrop"
    role="dialog"
    aria-modal="true"
    aria-labelledby="ply-export-title"
    aria-describedby="ply-export-description"
    onKeyDown={(event) => {
      if (event.key === "Escape" && !active) onClose();
    }}
  >
    <div className="gaussian-save-dialog gaussian-export-dialog">
      <div className={`gaussian-save-symbol ${state.phase}`}>
        {active ? <LoaderCircle className="spin" size={19} /> : state.phase === "completed" ? <FolderOpen size={19} /> : <X size={19} />}
      </div>
      <div id="ply-export-description">
        <h2 id="ply-export-title">{title}</h2>
        {state.phase === "preparing" && <p>{t("export.plyPreparing")}</p>}
        {state.phase === "exporting" && <>
          <p>{t("export.plyProgress", { value: Math.floor(progress) })}</p>
          <div className="gaussian-export-progress" role="status">
            <progress max={100} value={progress} />
            <b>{Math.floor(progress)}%</b>
          </div>
        </>}
        {state.phase === "completed" && <>
          <p>{t("export.plyCompletedDescription")}</p>
          <code className="gaussian-export-path">{state.path}</code>
          {state.revealError && <p className="gaussian-export-error">{state.revealError}</p>}
        </>}
        {state.phase === "error" && <p className="gaussian-export-error">{t("export.plyFailed", { detail: state.error })}</p>}
      </div>
      {state.phase === "completed" && <div className="gaussian-save-actions">
        <button autoFocus type="button" className="secondary" onClick={onClose}>{t("common.close")}</button>
        <button type="button" className="primary" onClick={() => onReveal(state.path)}><FolderOpen size={14} />{t("export.openFolder")}</button>
      </div>}
      {state.phase === "error" && <div className="gaussian-save-actions">
        <button autoFocus type="button" className="secondary" onClick={onClose}>{t("common.close")}</button>
        <button type="button" className="primary" onClick={onRetry}><Download size={14} />{t("export.plyRetry")}</button>
      </div>}
    </div>
  </div>;
}
