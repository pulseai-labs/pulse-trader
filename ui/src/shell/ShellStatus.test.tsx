// The pure helpers behind the title bar's status strip and the sidebar's status
// card (r7.s2.w4), and the shared "not built yet" state they render.

import { render, screen } from "@testing-library/react";
import { describe, expect, it } from "vitest";

import { NOT_BUILT_YET, NotBuiltYet } from "../components/NotBuiltYet";
import {
  CONNECTION_LABELS,
  connectionState,
  engineShort,
  serverHostPort,
  serverName,
} from "./ShellStatus";

describe("ShellStatus helpers (r7.s2.w4)", () => {
  it("maps up, down, not_connected and refused to the canvas's three states", () => {
    expect(connectionState("up")).toBe("connected");
    expect(connectionState("down")).toBe("reconnecting");
    expect(connectionState("not_connected")).toBe("disconnected");
    expect(connectionState("refused")).toBe("disconnected");
    expect(CONNECTION_LABELS).toEqual({
      connected: "connected",
      reconnecting: "reconnecting…",
      disconnected: "disconnected",
    });
  });

  it("names the server from the stored address, and says server without one", () => {
    expect(serverName("http://draco-desk:17620")).toBe("draco-desk");
    expect(serverName("http://100.90.203.21:8420")).toBe("100.90.203.21");
    expect(serverName(null)).toBe("server");
    expect(serverName(undefined)).toBe("server");
    expect(serverName("not a url")).toBe("server");
  });

  it("reads host and port from the address, and an em dash without one", () => {
    expect(serverHostPort("http://draco-desk:17620")).toBe("draco-desk:17620");
    expect(serverHostPort(null)).toBe("—");
    expect(serverHostPort("not a url")).toBe("—");
  });

  it("keeps the first seven characters of the engine fingerprint, and an em dash without one", () => {
    expect(engineShort("f0dbdf5748d38d5c")).toBe("f0dbdf5");
    expect(engineShort(null)).toBe("—");
  });

  it("renders not built yet with what it stands for in its title", () => {
    render(<NotBuiltYet what="Agent last seen" />);
    const chip = screen.getByText("not built yet");
    expect(chip.className).toContain("not-built");
    expect(chip.getAttribute("title")).toBe("Agent last seen — not built yet");
    expect(NOT_BUILT_YET).toBe("not built yet");
  });
});
