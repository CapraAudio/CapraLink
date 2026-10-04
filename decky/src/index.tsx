import { ButtonItem, DropdownItem, PanelSection, PanelSectionRow, SliderField, ToggleField, staticClasses } from "@decky/ui";
import { callable, definePlugin } from "@decky/api";
import { useCallback, useEffect, useRef, useState } from "react";
import { FaVolumeUp } from "react-icons/fa";

type Device = { id: string; name: string; online: boolean; connected: boolean };
type Status = {
  connected: { id: string; name: string } | null;
  devices: Device[];
  music_mode: boolean;
  quality: string | null;
  error: string | null;
  mute: boolean;
  send_volume: number;
  recv_volume: number;
  ptt: "off" | "hold" | "toggle";
  ptt_key: string | null;
  talking: boolean;
  ptt_error: string | null;
  hifi: boolean;
  delay_in_ms: number | null;
  delay_out_ms: number | null;
};
type Reply<T> = { ok: boolean; data: T | null; error: string };

const status = callable<[], Reply<Status>>("status");
const connect = callable<[device: string], Reply<null>>("connect");
const disconnect = callable<[], Reply<null>>("disconnect");
const music = callable<[on: boolean], Reply<null>>("music");
const toggle = callable<[flag: string, on: boolean], Reply<null>>("toggle");
const volume = callable<[which: string, n: number], Reply<null>>("volume");
const ptt = callable<[mode: string], Reply<null>>("ptt");
const pttSet = callable<[], Reply<null>>("ptt_set");

function Content() {
  const [st, setSt] = useState<Status | null>(null);
  const [error, setError] = useState("");

  const [capturing, setCapturing] = useState(false);
  const busy = useRef(false); // one CLI call at a time: polls never pile up behind a slow call

  const refresh = useCallback(async () => {
    if (busy.current) return;
    busy.current = true;
    const r = await status();
    busy.current = false;
    if (r.ok) setSt(r.data);
    setError(r.ok ? "" : r.error);
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [refresh]);

  const act = async (p: () => Promise<Reply<null>>) => {
    busy.current = true;
    const r = await p();
    busy.current = false;
    if (!r.ok) setError(r.error);
    else await refresh();
  };

  if (!st) {
    return (
      <PanelSection>
        <PanelSectionRow>{error || "Loading..."}</PanelSectionRow>
      </PanelSection>
    );
  }
  const title = st.connected ? `Connected to ${st.connected.name}${st.quality ? ` (${st.quality})` : ""}` : "Not connected";
  return (
    <PanelSection title={title}>
      {(error || st.error) && <PanelSectionRow>{error || st.error}</PanelSectionRow>}
      {st.delay_in_ms != null && st.delay_out_ms != null && (
        <PanelSectionRow>You hear them: {st.delay_in_ms} ms · They hear you: {st.delay_out_ms} ms</PanelSectionRow>
      )}
      {st.devices.map((d) => (
        <PanelSectionRow key={d.id}>
          <ButtonItem
            layout="below"
            disabled={!d.online && !d.connected}
            description={d.connected ? "Connected" : d.online ? "Online" : "Offline"}
            onClick={() => act(() => (d.connected ? disconnect() : connect(d.id)))}
          >
            {d.connected ? `Disconnect ${d.name}` : `Connect ${d.name}`}
          </ButtonItem>
        </PanelSectionRow>
      ))}
      <PanelSectionRow>
        <ToggleField label="Music Mode" checked={st.music_mode} onChange={(on) => act(() => music(on))} />
      </PanelSectionRow>
      <PanelSectionRow>
        <ToggleField label="Hi-Fi" checked={st.hifi} disabled={!st.music_mode} onChange={(on) => act(() => toggle("hifi", on))} />
      </PanelSectionRow>
      <PanelSectionRow>
        <ToggleField label="Mute" checked={st.mute} onChange={(on) => act(() => toggle("mute", on))} />
      </PanelSectionRow>
      <PanelSectionRow>
        <SliderField label="Send volume" value={st.send_volume} min={0} max={150} step={5} showValue onChange={(n) => act(() => volume("send", n))} />
      </PanelSectionRow>
      <PanelSectionRow>
        <SliderField label="Receive volume" value={st.recv_volume} min={0} max={150} step={5} showValue onChange={(n) => act(() => volume("recv", n))} />
      </PanelSectionRow>
      <PanelSection title="Push-to-talk">
        <PanelSectionRow>
          <DropdownItem
            label="Mode"
            disabled={!st.ptt_key}
            selectedOption={st.ptt}
            rgOptions={[
              { data: "off", label: "Off" },
              { data: "hold", label: "Hold" },
              { data: "toggle", label: "Toggle" },
            ]}
            onChange={(o) => act(() => ptt(o.data))}
          />
        </PanelSectionRow>
        <PanelSectionRow>
          <ButtonItem
            layout="below"
            disabled={capturing}
            description={st.ptt_key ? `Button: ${st.ptt_key}. Map a Deck button to a keyboard key in Steam Input, then press it here` : "Map a Deck button to a keyboard key in Steam Input, then press it here"}
            onClick={async () => {
              setCapturing(true);
              await act(pttSet);
              setCapturing(false);
            }}
          >
            {capturing ? "Press a button..." : "Set button"}
          </ButtonItem>
        </PanelSectionRow>
        {st.ptt_error && <PanelSectionRow><span style={{ color: "red" }}>{st.ptt_error}</span></PanelSectionRow>}
        {st.talking && <PanelSectionRow>Talking</PanelSectionRow>}
      </PanelSection>
    </PanelSection>
  );
}

export default definePlugin(() => ({
  name: "CapraLink",
  titleView: <div className={staticClasses.Title}>CapraLink</div>,
  content: <Content />,
  icon: <FaVolumeUp />,
}));
