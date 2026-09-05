import { useEffect, useState, type ReactNode } from "react";
import { invoke } from "@tauri-apps/api/core";
import {
  fileFolder,
  fileName,
  formatEta,
  formatRate,
} from "../lib/format";
import type { ScanState } from "../lib/types";

export function Empty({
  icon,
  text,
  detail,
  action,
}: {
  icon: string;
  text: string;
  detail: string;
  action?: { label: string; onClick: () => void };
}) {
  return (
    <div className="empty">
      <span>{icon}</span>
      <b>{text}</b>
      <p>{detail}</p>
      {action && (
        <button className="secondary" onClick={action.onClick}>
          {action.label}
        </button>
      )}
    </div>
  );
}

export type ConfirmOption = {
  label: string;
  kind?: "primary" | "secondary" | "danger";
  action: () => void;
};

// Unified confirmation dialog: explicit cancel button (the only exit in the
// earlier review dialogs was clicking the backdrop), Escape to close, and
// ARIA roles. All destructive flows go through this component.
export function ConfirmDialog({
  title,
  detail,
  note,
  options,
  busy,
  onClose,
}: {
  title: string;
  detail?: ReactNode;
  note?: string;
  options: ConfirmOption[];
  busy: boolean;
  onClose: () => void;
}) {
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onClick={onClose} role="presentation">
      <div
        className="modal modal-confirm"
        role="dialog"
        aria-modal="true"
        aria-label={title}
        onClick={(event) => event.stopPropagation()}
      >
        <h3>{title}</h3>
        {detail && <p>{detail}</p>}
        {note && <p className="modal-note">{note}</p>}
        <div className="modal-actions-row">
          {options.map((option) => (
            <button
              key={option.label}
              className={option.kind ?? "secondary"}
              disabled={busy}
              onClick={option.action}
            >
              {option.label}
            </button>
          ))}
          <button
            className="text-button modal-cancel"
            disabled={busy}
            onClick={onClose}
          >
            取消
          </button>
        </div>
      </div>
    </div>
  );
}

// In-app preview for images; every other type falls back to the "open with
// the system program" button.
export function PreviewModal({
  path,
  onClose,
}: {
  path: string;
  onClose: () => void;
}) {
  const [image, setImage] = useState<string | null>(null);
  const [failed, setFailed] = useState(false);
  useEffect(() => {
    setImage(null);
    setFailed(false);
    void invoke<string | null>("image_preview", { path })
      .then((result) => {
        if (result) setImage(result);
        else setFailed(true);
      })
      .catch(() => setFailed(true));
  }, [path]);
  useEffect(() => {
    const onKey = (event: KeyboardEvent) => {
      if (event.key === "Escape") onClose();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);
  return (
    <div className="modal-backdrop" onClick={onClose} role="presentation">
      <div
        className="modal"
        role="dialog"
        aria-modal="true"
        aria-label={`预览 ${fileName(path)}`}
        onClick={(event) => event.stopPropagation()}
      >
        <div className="modal-head">
          <div>
            <b>{fileName(path)}</b>
            <span>{fileFolder(path)}</span>
          </div>
          <div className="modal-actions">
            <button
              className="secondary"
              onClick={() =>
                void invoke("reveal_in_manager", { path }).catch((error) =>
                  console.error("reveal_in_manager failed:", error),
                )
              }
            >
              定位
            </button>
            <button
              className="secondary"
              onClick={() =>
                void invoke("open_file", { path }).catch((error) =>
                  console.error("open_file failed:", error),
                )
              }
            >
              用系统程序打开
            </button>
            <button className="secondary" onClick={onClose}>
              关闭
            </button>
          </div>
        </div>
        {image && <img className="preview-image" src={image} alt={fileName(path)} />}
        {failed && (
          <div className="preview-fallback">
            <b>此文件无法预览</b>
            <span>只有图片支持应用内预览，其他类型请用系统程序打开。</span>
          </div>
        )}
        {!image && !failed && <div className="preview-loading">正在加载预览...</div>}
      </div>
    </div>
  );
}

export function ScanProgress({
  scanState,
  history,
  onCancel,
  disabled,
}: {
  scanState: ScanState;
  history: { t: number; processed: number }[];
  onCancel: () => void;
  disabled: boolean;
}) {
  const percent =
    scanState.total > 0
      ? Math.min(100, Math.round((scanState.processed / scanState.total) * 100))
      : null;
  let rate: number | null = null;
  if (history.length >= 2) {
    const seconds = (history[history.length - 1].t - history[0].t) / 1000;
    const delta = history[history.length - 1].processed - history[0].processed;
    if (seconds > 0 && delta > 0) rate = delta / seconds;
  }
  const etaSeconds =
    rate !== null && scanState.total > scanState.processed
      ? (scanState.total - scanState.processed) / rate
      : null;
  const shownSamples = scanState.recent_errors.length;
  return (
    <section className="scan-progress">
      <div className="scan-progress-head">
        <b>后台扫描中</b>
        <span>
          已处理 {scanState.processed.toLocaleString()}
          {scanState.total > 0
            ? ` / ${scanState.total.toLocaleString()} 个条目`
            : " 个条目"}
          {rate !== null && ` · ${formatRate(rate)}`}
          {etaSeconds !== null && ` · ${formatEta(etaSeconds)}`}
        </span>
      </div>
      {scanState.current_path && (
        <div className="scan-current" title={scanState.current_path}>
          正在处理：{scanState.current_path}
        </div>
      )}
      {scanState.errors_total > 0 && (
        <details className="scan-errors">
          <summary>
            {scanState.errors_total} 个条目处理失败（点击查看
            {shownSamples > 0 && shownSamples < scanState.errors_total
              ? `最近 ${shownSamples} 条`
              : "详情"}
            ）
          </summary>
          <ul>
            {scanState.recent_errors.map((sample, index) => (
              <li key={index}>
                <span title={sample.path}>{fileName(sample.path)}</span>
                <small>{sample.error}</small>
              </li>
            ))}
          </ul>
          {shownSamples > 0 && shownSamples < scanState.errors_total && (
            <small className="scan-errors-hint">
              仅保留最近 {shownSamples} 条样本，其余失败原因相同或已被覆盖。
            </small>
          )}
        </details>
      )}
      <div className="progress-track">
        <i
          className={percent === null ? "indeterminate" : ""}
          style={percent === null ? undefined : { width: `${percent}%` }}
        />
      </div>
      <div className="scan-progress-foot">
        <span>
          {percent === null ? "正在统计文件数量..." : `${percent}%`}
        </span>
        <button className="secondary" disabled={disabled} onClick={onCancel}>
          取消扫描
        </button>
      </div>
    </section>
  );
}
