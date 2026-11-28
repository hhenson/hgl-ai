import { afterEach, expect, it, vi } from "vitest";

const { connect } = vi.hoisted(() => ({ connect: vi.fn() }));
vi.mock("@nats-io/nats-core", () => ({
  wsconnect: connect,
  nkeyAuthenticator: () => () => ({}),
}));

import { start } from "./hub";
import { Store } from "./store";

it("marks telemetry disconnected when session validation stops the hub", async () => {
  vi.stubGlobal("window", { location: { protocol: "https:", host: "console.example" } });
  const close = vi.fn(async () => {});
  connect.mockResolvedValue({
    close,
    isClosed: () => true,
    status: async function* () {},
    closed: () => new Promise(() => {}),
  });
  const store = new Store();
  const stop = start({ user: "viewer", seed: "seed", path: "/nats" }, store);
  await vi.waitFor(() => expect(store.meta.connection).toBe("connected"));
  await stop();
  expect(close).toHaveBeenCalledOnce();
  expect(store.meta.connection).toBe("disconnected");
});

afterEach(() => { vi.unstubAllGlobals(); vi.clearAllMocks(); });
