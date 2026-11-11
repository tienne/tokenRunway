export type InboxLevel = "done" | "waiting" | "blocked" | "interrupted" | "failed" | "turnEnd";

/** 응답이나 확인이 필요한 상태만 확인 필요 건수에 포함한다. */
export function inboxNeedsAttention(level: InboxLevel): boolean {
  return level === "waiting" || level === "blocked" || level === "interrupted" || level === "failed";
}

export type InboxTarget =
  | { kind: "orca"; worktreeId: string; worktreePath: string | null; tabId: string | null }
  | { kind: "paseo"; agentId: string; cwd: string | null }
  | { kind: "path"; path: string }
  | { kind: "none" };

export interface InboxQuestion {
  question: string;
  options: { label: string; description?: string | null }[];
  multiSelect?: boolean;
}

export interface InboxItem {
  id: string;
  title: string;
  body: string | null;
  prompt?: string | null;
  questions?: InboxQuestion[];
  level: InboxLevel;
  context: string | null;
  target: InboxTarget;
  projectPath?: string | null;
  createdMs: number;
  read: boolean;
}

export interface InboxGroup {
  key: string;
  label: string;
  path: string | null;
  items: InboxItem[];
  unread: number;
  attention: number;
}

function cleanPath(path: string): string {
  return path.replace(/\\/g, "/").replace(/\/+$/, "") || "/";
}

/** 공통 Git 저장소를 프로젝트로 묶고 안 읽은 그룹부터 최신순으로 정렬한다. */
export function groupInbox(items: InboxItem[], otherLabel: string): InboxGroup[] {
  const groups = new Map<string, InboxGroup>();
  for (const item of [...items].sort((a, b) => b.createdMs - a.createdMs)) {
    const target = item.target;
    const rawPath = item.projectPath || (target.kind === "orca" ? target.worktreePath
      : target.kind === "paseo" ? target.cwd
      : target.kind === "path" ? target.path : null);
    const path = rawPath ? cleanPath(rawPath) : null;
    const key = item.projectPath ? `project:${path}` : target.kind === "orca" ? `orca:${target.worktreeId}`
      : path ? `path:${path}`
      : target.kind === "paseo" ? `paseo:${target.agentId}` : "other";
    let group = groups.get(key);
    if (!group) {
      group = {
        key, path,
        label: path ? path.split("/").pop() || path
          : target.kind === "none" ? otherLabel : item.context || (target.kind === "orca" ? target.worktreeId : target.kind === "paseo" ? target.agentId : otherLabel),
        items: [], unread: 0, attention: 0,
      };
      groups.set(key, group);
    }
    group.items.push(item);
    if (!item.read) {
      group.unread++;
      if (inboxNeedsAttention(item.level)) group.attention++;
    }
  }
  return [...groups.values()].sort((a, b) =>
    Number(b.unread > 0) - Number(a.unread > 0)
    || b.items[0].createdMs - a.items[0].createdMs);
}
