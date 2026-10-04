import { ButtonItem, PanelSection, PanelSectionRow, ToggleField, staticClasses } from "@decky/ui";
import { callable, definePlugin } from "@decky/api";
import { useCallback, useEffect, useState } from "react";
import { FaVolumeUp } from "react-icons/fa";

type Device = { id: string; name: string; online: boolean; connected: boolean };
type Status = {
  connected: { id: string; name: string } | null;
  devices: Device[];
  music_mode: boolean;
  quality: string | null;
  error: string | null;
};
type Reply<T> = { ok: boolean; data: T | null; error: string };

const status = callable<[], Reply<Status>>("status");
const connect = callable<[device: string], Reply<null>>("connect");
const disconnect = callable<[], Reply<null>>("disconnect");
const music = callable<[on: boolean], Reply<null>>("music");

function Content() {
  const [st, setSt] = useState<Status | null>(null);
  const [error, setError] = useState("");

  const refresh = useCallback(async () => {
    const r = await status();
    if (r.ok) setSt(r.data);
    setError(r.ok ? "" : r.error);
  }, []);

  useEffect(() => {
    refresh();
    const t = setInterval(refresh, 2000);
    return () => clearInterval(t);
  }, [refresh]);

  const act = async (p: Promise<Reply<null>>) => {
    const r = await p;
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
      {st.devices.map((d) => (
        <PanelSectionRow key={d.id}>
          <ButtonItem
            layout="below"
            disabled={!d.online && !d.connected}
            description={d.connected ? "Connected" : d.online ? "Online" : "Offline"}
            onClick={() => act(d.connected ? disconnect() : connect(d.id))}
          >
            {d.connected ? `Disconnect ${d.name}` : `Connect ${d.name}`}
          </ButtonItem>
        </PanelSectionRow>
      ))}
      <PanelSectionRow>
        <ToggleField label="Music Mode" checked={st.music_mode} onChange={(on) => act(music(on))} />
      </PanelSectionRow>
    </PanelSection>
  );
}

export default definePlugin(() => ({
  name: "CapraLink",
  titleView: <div className={staticClasses.Title}>CapraLink</div>,
  content: <Content />,
  icon: <FaVolumeUp />,
}));
