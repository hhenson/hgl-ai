import { describe, expect, it } from "vitest";
import { hubUrl } from "./hub";

describe("hubUrl", () => {
  it("is the page's own origin", () => {
    expect(hubUrl({ protocol: "https:", host: "hub.example.ts.net" }, "/nats")).toBe("wss://hub.example.ts.net/nats");
    expect(hubUrl({ protocol: "http:", host: "127.0.0.1:8080" }, "/nats")).toBe("ws://127.0.0.1:8080/nats");
  });
});
