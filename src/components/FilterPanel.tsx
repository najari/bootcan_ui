import { useEffect, useRef, useState } from "react";
import { useShallow } from "zustand/react/shallow";
import { invoke } from "@tauri-apps/api/core";
import { traceBuses, useCanStore } from "../stores/canStore";
import { PlusIcon, XMarkIcon } from "./icons";

type Rule = { type: string; [key: string]: any };

function backendRule(f: Rule): object {
  switch (f.type) {
    case "idRange": return { IdRange: { min: f.idMin ?? 0, max: f.idMax ?? 0x7ff } };
    case "idExact": return { IdExact: f.idExact ?? 0 };
    case "dlcRange": return { DlcRange: { min: f.dlcMin ?? 0, max: f.dlcMax ?? 64 } };
    case "direction": return { Direction: { rx: !!f.rx, tx: !!f.tx } };
    case "extendedId": return { ExtendedId: !!f.extended };
    case "remoteFrame": return { RemoteFrame: !!f.remote };
    case "channel": return { Channel: f.bus ?? 1 };
    case "timeRange": return { TimeRange: { start: f.start ?? null, end: f.end ?? null } };
    case "dataPattern": {
      const tokens: string[] = (f.patternText ?? "").trim().split(/\s+/).filter(Boolean);
      if (tokens.length > 64 || tokens.some((t) => !/^(?:[0-9a-f]{2}|\?\?)$/i.test(t))) {
        throw new Error("Data bytes must be hex pairs or ?? (up to 64 bytes).");
      }
      return { DataPattern: { pattern: tokens.flatMap((t, position) => t === "??" ? [] : [{ position, value: parseInt(t, 16), mask: 255 }]) } };
    }
    default: throw new Error(`Unsupported filter: ${f.type}`);
  }
}

function HexInput({ value, onCommit }: { value: number; onCommit: (v: number) => void }) {
  const [text, setText] = useState(`0x${value.toString(16).toUpperCase()}`);
  useEffect(() => setText(`0x${value.toString(16).toUpperCase()}`), [value]);
  return <input aria-label="CAN ID" className="input min-w-0 flex-1 px-1 py-0.5 text-xxs font-mono" value={text}
    onChange={(e) => {
      setText(e.target.value);
      const t = e.target.value.trim().replace(/^0x/i, "");
      if (/^[0-9a-f]+$/i.test(t)) onCommit(parseInt(t, 16));
    }} />;
}

export function useDisplayFilterSync() {
  const { filters, filterLogic } = useCanStore(useShallow((s) => ({
    filters: s.filters, filterLogic: s.filterLogic,
  })));
  const nets = useCanStore(useShallow((s) => s.nets));
  const setError = (filterError: string | null) => useCanStore.setState({ filterError });
  const key = JSON.stringify({ filters, filterLogic, buses: traceBuses(nets) });
  const previous = useRef(key);

  useEffect(() => {
    let disposed = false;
    const timer = setTimeout(async () => {
      try {
        const rules = filters.map(backendRule);
        await invoke("set_display_filter", {
          filter: { rules, logic: filterLogic === "and" ? "And" : "Or" }, buses: traceBuses(nets),
        });
        if (disposed) return;
        if (previous.current !== key) {
          previous.current = key;
          useCanStore.setState({ monitorMessages: new Map(), expandedRows: new Set(), plotData: new Map() });
        }
        await useCanStore.getState().refreshFilteredTrace();
        if (!disposed) setError(null);
      } catch (e) {
        if (!disposed) setError(String(e));
      }
    }, 300);
    return () => { disposed = true; clearTimeout(timer); };
  // Rules and logical bus assignments determine the shared backend filter.
  // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [key]);

}

export function FilterPanel() {
  const { filters, filterLogic, setFilters, setFilterLogic, error } = useCanStore(useShallow((s) => ({
    filters: s.filters, filterLogic: s.filterLogic, setFilters: s.setFilters, setFilterLogic: s.setFilterLogic, error: s.filterError,
  })));
  const update = (index: number, patch: Partial<Rule>) => setFilters(filters.map((f, i) => i === index ? { ...f, ...patch } : f));
  const numberInput = (index: number, field: string, value: number | undefined, label: string, max?: number) => (
    <input aria-label={label} placeholder={label} type="number" min="0" max={max} step={field === "start" || field === "end" ? "any" : "1"}
      value={value ?? ""} onChange={(e) => update(index, { [field]: e.target.value === "" ? undefined : Number(e.target.value) })}
      className="input min-w-0 flex-1 px-1 py-0.5 text-xxs" />
  );

  return <div className="p-2 space-y-1.5 border-b border-can-border overflow-y-auto flex-shrink-0" style={{ maxHeight: "40vh", minHeight: "120px" }}>
    <div className="flex items-center justify-between">
      <h3 className="text-xs font-semibold uppercase tracking-wider text-can-text-secondary">Filters</h3>
      <button onClick={() => setFilters([...filters, { type: "idRange", idMin: 0, idMax: 0x7ff }])} className="btn btn-secondary text-xxs px-1.5 py-0.5">
        <PlusIcon className="w-3 h-3" /> Add
      </button>
    </div>
    {filters.length > 0 && <>
      <div className="flex items-center gap-1 text-xxs">
        Logic:
        <select aria-label="Filter logic" value={filterLogic} onChange={(e) => setFilterLogic(e.target.value as "and" | "or")} className="input px-1 py-0.5">
          <option value="and">AND</option><option value="or">OR</option>
        </select>
        <button className="btn btn-secondary ml-auto px-1 py-0.5" onClick={() => setFilters([])}>Clear</button>
      </div>
      {filters.map((f, index) => <div key={index} className="bg-can-bg-tertiary rounded px-1.5 py-1 space-y-1">
        <div className="flex gap-1">
          <select aria-label={`Filter ${index + 1} type`} value={f.type} onChange={(e) => update(index, { type: e.target.value })} className="input min-w-0 flex-1 text-xxs px-1 py-0.5">
            <option value="idRange">ID Range</option><option value="idExact">ID Exact</option>
            <option value="direction">Direction</option><option value="channel">Bus Channel</option>
            <option value="timeRange">Time Range (s)</option><option value="extendedId">Extended ID</option>
            <option value="remoteFrame">Remote Frame</option><option value="dlcRange">Data Length</option>
            <option value="dataPattern">Data Bytes</option>
          </select>
          <button aria-label={`Remove filter ${index + 1}`} onClick={() => setFilters(filters.filter((_, i) => i !== index))} className="text-can-text-muted hover:text-can-accent-red"><XMarkIcon className="w-3 h-3" /></button>
        </div>
        {f.type === "idRange" && <div className="flex gap-1"><HexInput value={f.idMin ?? 0} onCommit={(idMin) => update(index, { idMin })} /><HexInput value={f.idMax ?? 0x7ff} onCommit={(idMax) => update(index, { idMax })} /></div>}
        {f.type === "idExact" && <div className="flex"><HexInput value={f.idExact ?? 0} onCommit={(idExact) => update(index, { idExact })} /></div>}
        {f.type === "channel" && <div className="flex">{numberInput(index, "bus", f.bus ?? 1, "Bus channel (1-based)", 65535)}</div>}
        {f.type === "timeRange" && <div className="flex gap-1">{numberInput(index, "start", f.start, "Start (s)")}{numberInput(index, "end", f.end, "End (s)")}</div>}
        {f.type === "dlcRange" && <div className="flex gap-1">{numberInput(index, "dlcMin", f.dlcMin ?? 0, "Min bytes", 64)}{numberInput(index, "dlcMax", f.dlcMax ?? 64, "Max bytes", 64)}</div>}
        {f.type === "direction" && <div className="flex gap-2 text-xxs">{["rx", "tx"].map((direction) => <label key={direction} className="flex gap-1"><input type="checkbox" checked={!!f[direction]} onChange={(e) => update(index, { [direction]: e.target.checked })} />{direction.toUpperCase()}</label>)}</div>}
        {(f.type === "extendedId" || f.type === "remoteFrame") && <label className="flex gap-1 text-xxs"><input type="checkbox" checked={!!f[f.type === "extendedId" ? "extended" : "remote"]} onChange={(e) => update(index, { [f.type === "extendedId" ? "extended" : "remote"]: e.target.checked })} />{f.type === "extendedId" ? "Extended" : "Remote"} (unchecked: exclude)</label>}
        {f.type === "dataPattern" && <input aria-label="Data byte pattern" className="input w-full text-xxs font-mono px-1 py-0.5" placeholder="AA ?? 55" value={f.patternText ?? ""} onChange={(e) => update(index, { patternText: e.target.value })} />}
      </div>)}
    </>}
    {filters.length === 0 && <p className="text-xxs text-can-text-muted">All CAN frames are visible.</p>}
    <p className="text-xxs text-can-text-muted">Applies to live view, traces and playback. Time is log/connection offset; end is exclusive. The full recording is retained.</p>
    {error && <p role="alert" className="text-xxs text-can-accent-red break-words">{error}</p>}
  </div>;
}
