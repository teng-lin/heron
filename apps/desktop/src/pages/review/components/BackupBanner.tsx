import type { BackupInfo } from "../../../lib/invoke";
import { Button } from "../../../components/ui/button";
import { formatBackupTime } from "../utils/format";

/**
 * `.md.bak` restore pill. Surfaces the backup's timestamp + a
 * Restore button when a backup exists. Hidden by the parent when
 * `backup` is `null` — the renderer probes `heron_check_backup` on
 * every (vault, session) change.
 */
export function BackupBanner({
  backup,
  onRestore,
}: {
  backup: BackupInfo;
  onRestore: () => void;
}) {
  return (
    <div
      className="mb-4 flex items-center justify-between gap-2 rounded border px-3 py-2 text-xs"
      style={{
        background: "var(--color-paper-2)",
        borderColor: "var(--color-warn)",
        color: "var(--color-ink-2)",
      }}
    >
      <span>
        Backup from{" "}
        <span className="font-mono">{formatBackupTime(backup.created_at)}</span>
      </span>
      <Button type="button" variant="outline" size="sm" onClick={onRestore}>
        Restore
      </Button>
    </div>
  );
}
