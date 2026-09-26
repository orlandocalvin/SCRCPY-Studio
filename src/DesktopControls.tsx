import { invoke } from "@tauri-apps/api/core";
import {
  CheckCircle2,
  CircleAlert,
  FolderOpen,
  Info,
  RefreshCw,
  RotateCcw,
  Sparkles
} from "lucide-react";
import { useCallback, useEffect, useRef, useState } from "react";
import type {
  DesktopCapabilities,
  DesktopExperienceResult,
  DesktopProbeState,
  LaunchConfig,
  LaunchResult
} from "./types";
import "./desktop.css";

interface Props {
  serial: string;
  config: LaunchConfig;
  onChange: (next: LaunchConfig) => void;
  onStatus: (status: string) => void;
  onProbeStateChange: (state: DesktopProbeState) => void;
  lastLaunchResult?: LaunchResult | null;
}

export default function DesktopControls({ serial, config, onChange, onStatus, onProbeStateChange, lastLaunchResult }: Props) {
  const [capabilities, setCapabilities] = useState<DesktopCapabilities | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const activeSerial = useRef(serial);
  const inFlightProbe = useRef<{ serial: string; request: Promise<DesktopCapabilities> } | null>(null);
  // A probe takes several seconds; merge its result into the latest config,
  // not the one captured when it started, so edits made meanwhile survive.
  const latestConfig = useRef(config);
  latestConfig.current = config;

  const updateConfig = useCallback((patch: Partial<LaunchConfig>) => {
    const current = latestConfig.current;
    if (current.serial !== serial || current.mode !== "desktop") return;
    onChange({ ...current, ...patch });
  }, [onChange, serial]);

  const applyCapabilities = useCallback((result: DesktopCapabilities) => {
    setCapabilities(result);
    setError(null);
    const current = latestConfig.current;
    const firstResult = !current.desktopEnvironment || current.desktopEnvironment === "unavailable";
    // Keep the user's choices; only turn off options this device cannot use.
    updateConfig({
      desktopEnvironment: result.environmentKind,
      desktopDisplayId: result.existingDisplayId ?? null,
      ...(firstResult ? {
        desktopWidth: result.recommendedWidth,
        desktopHeight: result.recommendedHeight,
        desktopDensity: result.recommendedDensity
      } : {}),
      desktopFlex: result.flexSupported && Boolean(current.desktopFlex),
      desktopNoDecorations: result.systemDecorationsSupported && Boolean(current.desktopNoDecorations),
      desktopKeepContent: result.keepContentSupported && Boolean(current.desktopKeepContent)
    });
    onProbeStateChange({ serial, checking: false, capabilities: result, error: null });
    onStatus(result.message);
  }, [onProbeStateChange, onStatus, serial, updateConfig]);

  const probe = useCallback(async () => {
    setBusy(true);
    setError(null);
    onProbeStateChange({ serial, checking: true, capabilities: null, error: null });
    onStatus("Creating a temporary display and checking its real Android windowing state…");
    // Share a probe already running for this phone (React runs mount effects
    // twice in development) instead of starting a second one.
    const pending = inFlightProbe.current;
    const request = pending?.serial === serial
      ? pending.request
      : invoke<DesktopCapabilities>("probe_desktop_capabilities", { serial });
    inFlightProbe.current = { serial, request };
    try {
      const result = await request;
      if (activeSerial.current === serial) applyCapabilities(result);
      return result;
    } catch (probeError) {
      const message = String(probeError);
      if (activeSerial.current === serial) {
        setCapabilities(null);
        setError(message);
        updateConfig({ desktopEnvironment: "unavailable", desktopDisplayId: null });
        onProbeStateChange({ serial, checking: false, capabilities: null, error: message });
        onStatus(`Desktop check failed: ${message}`);
      }
      return null;
    } finally {
      if (inFlightProbe.current?.request === request) inFlightProbe.current = null;
      if (activeSerial.current === serial) setBusy(false);
    }
  }, [applyCapabilities, onProbeStateChange, onStatus, serial, updateConfig]);

  useEffect(() => {
    activeSerial.current = serial;
    setCapabilities(null);
    setError(null);
    void probe();
    return () => {
      activeSerial.current = "";
    };
    // A new serial must trigger one evidence probe. Config is intentionally not a dependency.
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serial]);

  const waitForRebootAndProbe = async (message: string) => {
    onStatus(message);
    const deadline = Date.now() + 180_000;
    while (Date.now() < deadline && activeSerial.current === serial) {
      await new Promise((resolve) => window.setTimeout(resolve, 3000));
      try {
        const result = await invoke<DesktopCapabilities>("probe_desktop_capabilities", { serial });
        applyCapabilities(result);
        return;
      } catch {
        onStatus("Phone is restarting. Waiting for ADB to reconnect…");
      }
    }
    throw new Error("The phone did not reconnect within three minutes. Reconnect it, then choose Recheck.");
  };

  const changeDeveloperSettings = async (command: "enable_desktop_experience" | "restore_desktop_experience") => {
    setBusy(true);
    setError(null);
    try {
      const result = await invoke<DesktopExperienceResult>(command, { serial });
      if (result.rebootStarted) {
        await waitForRebootAndProbe(result.message);
      } else {
        await probe();
      }
    } catch (changeError) {
      const message = String(changeError);
      setError(message);
      onStatus(message);
    } finally {
      setBusy(false);
    }
  };

  const openDiagnostics = async () => {
    try {
      const folder = await invoke<string>("open_desktop_diagnostics");
      onStatus(`Opened Desktop Diagnostics: ${folder}`);
    } catch (openError) {
      onStatus(`Could not open Desktop Diagnostics: ${String(openError)}`);
    }
  };

  return (
    <div className="desktop-controls">
      <div className="desktop-heading">
        <div>
          <span className="eyebrow">DISPLAY ENVIRONMENT</span>
          <strong>{capabilities?.environmentLabel ?? (busy ? "Checking…" : "Not checked")}</strong>
        </div>
        <span className={`desktop-state ${capabilities?.supported ? "ready" : "waiting"}`}>
          {busy ? <RefreshCw size={14} className="spin" /> : capabilities?.supported ? <CheckCircle2 size={14} /> : <CircleAlert size={14} />}
          {busy ? "Inspecting" : capabilities?.supported ? "Ready" : "Unavailable"}
        </span>
      </div>

      {busy && !capabilities && (
        <div className="inline-state"><RefreshCw size={16} className="spin" /> Checking display support</div>
      )}

      {error && <div className="finding error"><CircleAlert size={18} /><div><strong>Desktop check failed</strong><span>{error}</span></div></div>}

      {capabilities && (
        <>
          {capabilities.desktopExperienceCanPrepare && (
            <div className="desktop-setup">
              <button className="secondary wide" onClick={() => void changeDeveloperSettings("enable_desktop_experience")} disabled={busy}>
                {busy ? <RefreshCw size={16} className="spin" /> : <Sparkles size={16} />} Enable Freeform Windows & Restart
              </button>
            </div>
          )}

          {capabilities.desktopExperienceBackupAvailable && (
            <button className="secondary wide" onClick={() => void changeDeveloperSettings("restore_desktop_experience")} disabled={busy}>
              {busy ? <RefreshCw size={16} className="spin" /> : <RotateCcw size={16} />} Restore Original Android Settings & Restart
            </button>
          )}

          {capabilities.needsStartApp && !config.desktopStartApp?.trim() && (
            <div className="finding info"><Info size={18} /><div><strong>Choose an app to start</strong><span>This phone shows a blank screen on new displays. Set Start app package in More settings, for example com.android.settings.</span></div></div>
          )}

          {lastLaunchResult && !lastLaunchResult.started && (
            <div className="finding error"><CircleAlert size={18} /><div><strong>Launch did not stay running</strong><span>{lastLaunchResult.message}</span></div></div>
          )}

        </>
      )}

      <div className="desktop-actions">
        <button className="secondary" onClick={() => void probe()} disabled={busy}><RefreshCw size={16} className={busy ? "spin" : ""} /> Recheck</button>
        <button className="secondary" onClick={() => void openDiagnostics()}><FolderOpen size={16} /> Open Logs</button>
      </div>
    </div>
  );
}
