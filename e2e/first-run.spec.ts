import { expect, test } from "@playwright/test";
import { ipcCalls, mockDesktop } from "./fixtures";
import { fixtureSnapshot, fixtureStatus } from "../src/test/fixtures";
import { translator } from "../src/i18n";

for (const language of ["en", "zh"] as const) {
  const t = translator(language);
  test(`missing Core guides installation before creation in ${language}`, async ({
    page,
  }) => {
    const snapshot = fixtureSnapshot({ empty: true });
    snapshot.coreAvailable = false;
    snapshot.settings.language = language;
    await mockDesktop(page, { snapshot });
    await page.goto("/");
    await expect(
      page.getByRole("button", { name: t("Create your first workspace") }),
    ).toHaveCount(0);
    await page
      .locator(".welcome-title")
      .getByRole("button", { name: t("Install core") })
      .click();
    const version = page.getByLabel(t("Version to install"));
    await expect(version).toBeFocused();
    await expect(version).toHaveAttribute(
      "placeholder",
      t("For example, 0.5.0"),
    );
    expect(await ipcCalls(page, "save_workspace")).toHaveLength(0);
    expect(await ipcCalls(page, "start_workspace")).toHaveLength(0);
    await page.screenshot({
      path: `artifacts/first-run-${language}-mocked-ipc.png`,
      fullPage: true,
      animations: "disabled",
    });
    await version.fill(" 0.5.0 ");
    await page
      .getByRole("button", { name: t("Install version"), exact: true })
      .click();
    await expect(
      page.getByRole("button", { name: t("Create your first workspace") }),
    ).toBeVisible();
    await page
      .getByRole("button", { name: t("Create your first workspace") })
      .click();
    const dialog = page.getByRole("dialog", { name: t("New workspace") });
    await dialog.getByLabel(t("Workspace name")).fill("New project");
    await dialog
      .getByLabel(t("Folder path"))
      .fill("/test-fixtures/new-project");
    await dialog
      .getByRole("button", { name: t("Continue"), exact: true })
      .click();
    await dialog
      .getByRole("button", { name: t("Continue"), exact: true })
      .click();
    await dialog.getByRole("button", { name: t("Create and start") }).click();
    await expect(dialog).toBeHidden();
    expect(await ipcCalls(page, "save_workspace")).toHaveLength(1);
    expect(await ipcCalls(page, "start_workspace")).toHaveLength(1);
  });

  test(`offline configuration copy discloses connection readiness in ${language}`, async ({
    page,
  }) => {
    const snapshot = fixtureSnapshot({ stopped: true });
    snapshot.settings.language = language;
    snapshot.statuses[0].localEndpoint = fixtureStatus.localEndpoint;
    await mockDesktop(page, { snapshot });
    await page.goto("/");
    await page
      .getByRole("navigation")
      .getByRole("button", { name: t("Connections"), exact: true })
      .click();
    const local = page.locator(".connection-endpoint").first();
    const warning = t(
      "Local service is offline. You can copy this configuration, but start the workspace before connecting a client.",
    );
    await expect(local.getByText(warning)).toBeVisible();
    await local
      .getByRole("button", { name: t("Copy config"), exact: true })
      .click();
    await expect(
      page.getByRole("region", { name: t("Operation feedback") }),
    ).toContainText(warning);
    expect(await ipcCalls(page, "start_workspace")).toHaveLength(0);
    await local.getByRole("button", { name: t("View configuration") }).click();
    const dialog = page.getByRole("dialog", {
      name: t("Configure your client"),
    });
    await expect(dialog.getByText(warning)).toBeVisible();
    await page.screenshot({
      path: `artifacts/offline-copy-${language}-mocked-ipc.png`,
      fullPage: true,
      animations: "disabled",
    });
    await dialog
      .getByRole("button", { name: t("Copy config"), exact: true })
      .click();
    expect(await ipcCalls(page, "save_workspace")).toHaveLength(0);
  });
}
