async (page) => {
  await page.evaluate(async () => {
    const listing = await (await fetch('/bench/')).text();
    const names = [...listing.matchAll(/href="([^"]+\.ARW)"/gi)].map(m => decodeURIComponent(m[1])).slice(0, 3);
    const opfs = await navigator.storage.getDirectory();
    try { await opfs.removeEntry('export', { recursive: true }); } catch (e) {}
    const root = await opfs.getDirectoryHandle('export', { create: true });
    for (const name of names) {
      const bytes = await (await fetch('/bench/' + encodeURIComponent(name))).arrayBuffer();
      const w = await (await root.getFileHandle(name, { create: true })).createWritable();
      await w.write(bytes); await w.close();
    }
    window.__lpTestRoot = root;
  });
  const canvas = page.locator('canvas');
  await canvas.click({ position: { x: 800, y: 900 } });
  await page.keyboard.press('Meta+O');
  await page.waitForTimeout(4000);
  await page.keyboard.press('Meta+A');
  await page.keyboard.press('x');
  await page.waitForTimeout(500);
  const t0 = Date.now();
  await page.keyboard.press('Enter');
  const exported = await page.evaluate(async () => {
    const opfs = await navigator.storage.getDirectory();
    const root = await opfs.getDirectoryHandle('export');
    for (let i = 0; i < 240; i++) {
      try {
        const dir = await root.getDirectoryHandle('Exports');
        const out = [];
        for await (const [name, h] of dir.entries()) {
          if (h.kind === 'file' && name.endsWith('.jpg')) {
            const f = await h.getFile();
            const bmp = await createImageBitmap(f).catch(() => null);
            out.push({ name, bytes: f.size, w: bmp && bmp.width, h: bmp && bmp.height });
          }
        }
        if (out.length) return out;
      } catch (e) {}
      await new Promise(r => setTimeout(r, 500));
    }
    return [];
  });
  return { exported, ms: Date.now() - t0 };
}
