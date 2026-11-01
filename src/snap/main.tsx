// 统一截图遮罩窗（macOS / Windows 共用）：
// 整屏冻结图作背景 → 拖选/微调/吸附 → 选区旁动作条（复制图片 / 提取文字 / 翻译 / 钉图）。
//
// 坐标约定：所有 rect 都是本窗口的 CSS 坐标（= 显示器逻辑坐标）；Rust 侧按
// 冻结图实际像素 / 逻辑宽度换算裁剪。尺寸标签显示物理像素（与导出的图片一致）。
//
// 交互对位常见截图工具：Esc 取消、Enter 复制、双击选区内复制；点选区外
// 空白处重新框选；选区可整体拖动，八个手柄可改边。
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import type { MouseEvent as ReactMouseEvent, ReactNode } from "react";
import { createRoot } from "react-dom/client";
import { convertFileSrc, invoke } from "@tauri-apps/api/core";
import { Copy, Languages, Pin, ScanSearch, X } from "lucide-react";
import "./snap.css";

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

interface SessionPayload {
  image: string;
  width: number;
  height: number;
  scale: number;
  windows: Rect[];
}

interface Session extends Omit<SessionPayload, "image"> {
  imageUrl: string;
}

type Handle = "nw" | "n" | "ne" | "e" | "se" | "s" | "sw" | "w";

type Drag =
  | { mode: "new"; sx: number; sy: number; cx: number; cy: number }
  | { mode: "move"; sx: number; sy: number; orig: Rect }
  | { mode: "resize"; handle: Handle; sx: number; sy: number; orig: Rect };

type ActionId = "copy" | "extract" | "translate" | "pin";

/** 吸附容差（逻辑 px）：靠近窗口/文字行/屏幕边缘时贴上去 */
const SNAP_PX = 8;
const MIN_SIZE = 6;

function clamp(v: number, min: number, max: number) {
  return Math.min(max, Math.max(min, v));
}

function inside(p: { x: number; y: number }, r: Rect) {
  return p.x >= r.x && p.x <= r.x + r.w && p.y >= r.y && p.y <= r.y + r.h;
}

function normDrag(a: { x: number; y: number }, b: { x: number; y: number }): Rect {
  return {
    x: Math.min(a.x, b.x),
    y: Math.min(a.y, b.y),
    w: Math.abs(b.x - a.x),
    h: Math.abs(b.y - a.y),
  };
}

/** 单边吸附：返回最近的候选边坐标（无候选在容差内则原值） */
function snapEdge(v: number, candidates: number[], tol = SNAP_PX) {
  let best = v;
  let dist = tol;
  for (const c of candidates) {
    const d = Math.abs(v - c);
    if (d < dist) {
      dist = d;
      best = c;
    }
  }
  return best;
}

/** 移动整体选区时的吸附位移：左/右（上/下）边任一条靠近候选边即对齐 */
function snapDelta(start: number, end: number, candidates: number[], tol = SNAP_PX) {
  let best = 0;
  let dist = tol;
  for (const c of candidates) {
    const ds = Math.abs(start - c);
    if (ds < dist) {
      dist = ds;
      best = c - start;
    }
    const de = Math.abs(end - c);
    if (de < dist) {
      dist = de;
      best = c - end;
    }
  }
  return best;
}

const HANDLES: Array<[Handle, number, number]> = [
  ["nw", 0, 0],
  ["n", 0.5, 0],
  ["ne", 1, 0],
  ["e", 1, 0.5],
  ["se", 1, 1],
  ["s", 0.5, 1],
  ["sw", 0, 1],
  ["w", 0, 0.5],
];

const CURSORS: Record<Handle, string> = {
  nw: "nwse-resize",
  n: "ns-resize",
  ne: "nesw-resize",
  e: "ew-resize",
  se: "nwse-resize",
  s: "ns-resize",
  sw: "nesw-resize",
  w: "ew-resize",
};

const ACTIONS: Array<{ id: ActionId; label: string; icon: ReactNode }> = [
  { id: "copy", label: "复制图片", icon: <Copy size={15} strokeWidth={1.75} /> },
  { id: "extract", label: "提取文字", icon: <ScanSearch size={15} strokeWidth={1.75} /> },
  { id: "translate", label: "翻译", icon: <Languages size={15} strokeWidth={1.75} /> },
  { id: "pin", label: "钉图", icon: <Pin size={15} strokeWidth={1.75} /> },
];

function Overlay() {
  const [session, setSession] = useState<Session | null>(null);
  const [selection, setSelection] = useState<Rect | null>(null);
  const [drag, setDrag] = useState<Drag | null>(null);
  const [texts, setTexts] = useState<Rect[]>([]);
  const [busy, setBusy] = useState<ActionId | null>(null);
  const [error, setError] = useState("");

  const selectionRef = useRef<Rect | null>(null);
  const busyRef = useRef<ActionId | null>(null);
  selectionRef.current = selection;
  busyRef.current = busy;

  useEffect(() => {
    let alive = true;
    invoke<SessionPayload>("snap_session")
      .then((p) => {
        if (alive) {
          setSession({ ...p, imageUrl: convertFileSrc(p.image) });
        }
      })
      .catch((e) => {
        if (!alive) return;
        setError(String(e));
        window.setTimeout(() => {
          invoke("snap_cancel").catch(() => undefined);
        }, 1200);
      });
    return () => {
      alive = false;
    };
  }, []);

  // 文字行矩形由 Rust 后台跑 OCR 计算；就绪前吸附只用窗口/屏幕边缘
  useEffect(() => {
    if (!session) return;
    let alive = true;
    const fetchBlocks = () => {
      invoke<Rect[]>("snap_text_rects")
        .then((r) => {
          if (alive && r && r.length) setTexts(r);
        })
        .catch(() => undefined);
    };
    fetchBlocks();
    const timer = window.setInterval(fetchBlocks, 400);
    const stop = window.setTimeout(() => window.clearInterval(timer), 12000);
    return () => {
      alive = false;
      window.clearInterval(timer);
      window.clearTimeout(stop);
    };
  }, [session]);

  const vCands = useMemo(() => {
    if (!session) return [];
    const out = [0, session.width];
    for (const r of session.windows) out.push(r.x, r.x + r.w);
    for (const r of texts) out.push(r.x, r.x + r.w);
    return out;
  }, [session, texts]);

  const hCands = useMemo(() => {
    if (!session) return [];
    const out = [0, session.height];
    for (const r of session.windows) out.push(r.y, r.y + r.h);
    for (const r of texts) out.push(r.y, r.y + r.h);
    return out;
  }, [session, texts]);

  const cancel = useCallback(() => {
    invoke("snap_cancel").catch(() => undefined);
  }, []);

  const runAction = useCallback((id: ActionId) => {
    if (!session || !selectionRef.current || busyRef.current) return;
    const r = selectionRef.current;
    setBusy(id);
    setError("");
    invoke("snap_action", {
      action: id,
      x: r.x,
      y: r.y,
      w: r.w,
      h: r.h,
    }).catch((e) => {
      setBusy(null);
      setError(String(e));
    });
  }, [session]);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") {
        e.preventDefault();
        cancel();
      } else if (e.key === "Enter" && selectionRef.current && !busyRef.current) {
        e.preventDefault();
        runAction("copy");
      }
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cancel, runAction]);

  const startNew = (x: number, y: number) => {
    const sx = snapEdge(x, vCands);
    const sy = snapEdge(y, hCands);
    setSelection(null);
    setDrag({ mode: "new", sx, sy, cx: sx, cy: sy });
  };

  const onMouseDown = (e: ReactMouseEvent) => {
    if (e.button !== 0 || !session) return;
    const p = { x: e.clientX, y: e.clientY };
    if (selection && inside(p, selection)) {
      setDrag({ mode: "move", sx: p.x, sy: p.y, orig: selection });
      return;
    }
    startNew(p.x, p.y);
  };

  const onMouseMove = (e: ReactMouseEvent) => {
    if (!drag || !session) return;
    const x = e.clientX;
    const y = e.clientY;

    if (drag.mode === "new") {
      setDrag({
        ...drag,
        cx: snapEdge(x, vCands),
        cy: snapEdge(y, hCands),
      });
      return;
    }

    if (drag.mode === "move") {
      const rawDx = x - drag.sx;
      const rawDy = y - drag.sy;
      const dx = snapDelta(
        drag.orig.x + rawDx,
        drag.orig.x + drag.orig.w + rawDx,
        vCands
      );
      const dy = snapDelta(
        drag.orig.y + rawDy,
        drag.orig.y + drag.orig.h + rawDy,
        hCands
      );
      const nx = clamp(drag.orig.x + rawDx + dx, 0, session.width - drag.orig.w);
      const ny = clamp(drag.orig.y + rawDy + dy, 0, session.height - drag.orig.h);
      setSelection({ x: nx, y: ny, w: drag.orig.w, h: drag.orig.h });
      return;
    }

    // resize：只有被拖动的手柄涉及的边移动
    const o = drag.orig;
    let left = o.x;
    let right = o.x + o.w;
    let top = o.y;
    let bottom = o.y + o.h;
    if (drag.handle.includes("w")) {
      left = clamp(snapEdge(x, vCands), 0, right - MIN_SIZE);
    }
    if (drag.handle.includes("e")) {
      right = clamp(snapEdge(x, vCands), left + MIN_SIZE, session.width);
    }
    if (drag.handle.includes("n")) {
      top = clamp(snapEdge(y, hCands), 0, bottom - MIN_SIZE);
    }
    if (drag.handle.includes("s")) {
      bottom = clamp(snapEdge(y, hCands), top + MIN_SIZE, session.height);
    }
    setSelection({ x: left, y: top, w: right - left, h: bottom - top });
  };

  const onMouseUp = (e?: ReactMouseEvent) => {
    if (!drag) return;
    if (drag.mode === "new") {
      // 用松手时的真实坐标收尾（最后一次 mousemove 未必落在松手点）
      const cx = e ? snapEdge(e.clientX, vCands) : drag.cx;
      const cy = e ? snapEdge(e.clientY, hCands) : drag.cy;
      const r = normDrag({ x: drag.sx, y: drag.sy }, { x: cx, y: cy });
      if (r.w < MIN_SIZE || r.h < MIN_SIZE) {
        setSelection(null);
      } else {
        setSelection(r);
      }
    }
    setDrag(null);
  };

  if (!session) {
    return <div className="snap-root" />;
  }

  const live: Rect | null =
    drag?.mode === "new"
      ? normDrag({ x: drag.sx, y: drag.sy }, { x: drag.cx, y: drag.cy })
      : selection;

  // 动作条：优先放选区下方，放不下翻到上方；水平居中并对边界钳制
  const BAR_W = 380;
  const BAR_H = 44;
  const GAP = 10;
  const barLeft = live
    ? clamp(live.x + live.w / 2 - BAR_W / 2, 8, session.width - BAR_W - 8)
    : 0;
  const barAbove = live ? live.y + live.h + GAP + BAR_H > session.height : false;
  const barTop = live
    ? barAbove
      ? Math.max(8, live.y - BAR_H - GAP)
      : live.y + live.h + GAP
    : 0;

  const cursor = drag
    ? drag.mode === "move"
      ? "move"
      : drag.mode === "resize"
        ? CURSORS[drag.handle]
        : "crosshair"
    : live
      ? "move"
      : "crosshair";

  return (
    <div
      className="snap-root"
      style={{ cursor }}
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={(e) => onMouseUp(e)}
      // 多显示器：拖到别的屏幕时本窗收不到 mouseUp，按最后位置落定，
      // 避免拖拽状态卡死（v1 只在光标所在屏框选）
      onMouseLeave={() => {
        if (drag) onMouseUp();
      }}
      onDoubleClick={(e) => {
        if (live && inside({ x: e.clientX, y: e.clientY }, live)) runAction("copy");
      }}
      onContextMenu={(e) => {
        e.preventDefault();
        cancel();
      }}
    >
      <img className="snap-image" src={session.imageUrl} alt="" draggable={false} />
      {!live && <div className="snap-dim" />}

      {live && (
        <>
          <div
            className="snap-sel"
            style={{ left: live.x, top: live.y, width: live.w, height: live.h }}
          />
          <div
            className="snap-size"
            style={{
              left: live.x,
              // 动作条翻到选区上方时，尺寸标签收进选区里，避免与动作条叠在一起
              top: barAbove ? live.y + 4 : Math.max(4, live.y - 26),
            }}
          >
            {Math.round(live.w * session.scale)} × {Math.round(live.h * session.scale)}
          </div>
          {HANDLES.map(([h, fx, fy]) => (
            <div
              key={h}
              className="snap-handle"
              style={{
                left: live.x + live.w * fx - 4,
                top: live.y + live.h * fy - 4,
                cursor: CURSORS[h],
              }}
              onMouseDown={(e) => {
                e.stopPropagation();
                e.preventDefault();
                setDrag({
                  mode: "resize",
                  handle: h,
                  sx: e.clientX,
                  sy: e.clientY,
                  orig: live,
                });
              }}
            />
          ))}
        </>
      )}

      {!live && <div className="snap-hint">拖动框选 · Esc 取消</div>}

      {live && !drag && (
        <div
          className="snap-bar"
          style={{ left: barLeft, top: barTop, width: BAR_W }}
          onMouseDown={(e) => e.stopPropagation()}
          onDoubleClick={(e) => e.stopPropagation()}
        >
          {ACTIONS.map((a) => (
            <button
              key={a.id}
              className={`snap-btn${busy === a.id ? " busy" : ""}`}
              disabled={busy !== null}
              onClick={() => runAction(a.id)}
            >
              {a.icon}
              <span>{busy === a.id ? "处理中…" : a.label}</span>
            </button>
          ))}
          <div className="snap-sep" />
          <button className="snap-btn close" aria-label="取消" onClick={cancel}>
            <X size={15} strokeWidth={1.75} />
          </button>
        </div>
      )}

      {error && <div className="snap-error">{error}</div>}
    </div>
  );
}

createRoot(document.getElementById("root")!).render(<Overlay />);
