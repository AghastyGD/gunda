import { cleanup, fireEvent, render, screen } from "@testing-library/svelte";
import { afterEach, describe, expect, it } from "vitest";
import ThemeControl from "$lib/ThemeControl.svelte";

afterEach(() => {
  cleanup();
  localStorage.clear();
  delete document.documentElement.dataset.theme;
});

describe("theme preference", () => {
  it("defaults to System and persists local choices", async () => {
    render(ThemeControl);
    const control = screen.getByRole("combobox", { name: "Colour theme" }) as HTMLSelectElement;
    expect(control.value).toBe("system");
    expect(document.documentElement.dataset.theme).toBe("system");
    await fireEvent.change(control, { target: { value: "light" } });
    expect(localStorage.getItem("gunda-theme")).toBe("light");
    expect(document.documentElement.dataset.theme).toBe("light");
    await fireEvent.change(control, { target: { value: "dark" } });
    expect(localStorage.getItem("gunda-theme")).toBe("dark");
    expect(document.documentElement.dataset.theme).toBe("dark");
  });

  it("restores a saved preference and returns to System", async () => {
    localStorage.setItem("gunda-theme", "dark");
    render(ThemeControl);
    const control = screen.getByRole("combobox", { name: "Colour theme" }) as HTMLSelectElement;
    expect(control.value).toBe("dark");
    await fireEvent.change(control, { target: { value: "system" } });
    expect(localStorage.getItem("gunda-theme")).toBe("system");
    expect(document.documentElement.dataset.theme).toBe("system");
  });
});
