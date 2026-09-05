// Formatting and error-translation helpers shared by every view.

export const LARGE_FILE_THRESHOLD = 1024 * 1024 * 1024;

export function formatBytes(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  const units = ["KB", "MB", "GB", "TB"];
  let value = bytes / 1024;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit++;
  }
  return `${value.toFixed(value >= 10 ? 0 : 1)} ${units[unit]}`;
}

export function formatFileTime(unixSeconds: number) {
  return new Date(unixSeconds * 1000).toLocaleString("zh-CN", {
    year: "numeric",
    month: "2-digit",
    day: "2-digit",
    hour: "2-digit",
    minute: "2-digit",
  });
}

export function formatRate(rate: number) {
  return `${rate.toLocaleString("zh-CN", { maximumFractionDigits: 0 })} 个/秒`;
}

export function formatEta(seconds: number) {
  if (seconds < 60) return `预计剩余 ${Math.ceil(seconds)} 秒`;
  const minutes = Math.floor(seconds / 60);
  if (minutes < 60) return `预计剩余 ${minutes} 分 ${Math.ceil(seconds % 60)} 秒`;
  return `预计剩余 ${Math.floor(minutes / 60)} 时 ${minutes % 60} 分`;
}

export function fileFolder(path: string) {
  const parts = path.split(/[\\/]/);
  return parts.slice(0, -1).join("/");
}

export function fileName(path: string) {
  return path.split(/[\\/]/).pop() ?? path;
}

// Core-library errors are English phrases; translate the known classes into
// Chinese guidance and fall back to a generic prefix.
export function friendlyError(error: unknown) {
  const raw = String(error);
  const rules: [RegExp, string][] = [
    [/permission denied|access is denied/i, "没有访问权限，请检查文件或目录的读取权限。"],
    [/database is locked|database table is locked/i, "数据库正被其他操作占用，请稍后重试。"],
    [/no such file|os error 2/i, "文件或目录不存在，可能已被移动或删除。"],
    [/no longer matches indexed hash/, "文件内容与索引记录不一致，请重新扫描后再试。"],
    [/file changed while hashing/, "扫描期间文件内容发生了变化，将在下次扫描时重试。"],
    [/must be an approved/, "只有已确认的精确重复副本才能移入回收站。"],
    [/is not a directory/, "扫描目录无效或已不存在，请重新添加。"],
    [/restore refused/, "原位置已存在文件，恢复被拒绝以避免覆盖。"],
    [/unknown file id/, "该文件已不在索引中，请刷新后重试。"],
  ];
  for (const [pattern, text] of rules) {
    if (pattern.test(raw)) return text;
  }
  return `操作失败：${raw}`;
}
