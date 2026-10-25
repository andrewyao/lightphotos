async (page) => {
  // The shared wasm memory is created by the page's JS glue, so wrap the
  // constructor before a reload to keep a handle on it.
  await page.addInitScript(() => {
    const Memory = WebAssembly.Memory;
    WebAssembly.Memory = function (d) {
      const m = new Memory(d);
      if (d && d.shared) window.__lpMemory = m;
      return m;
    };
    WebAssembly.Memory.prototype = Memory.prototype;
  });
  await page.reload();
  await page.waitForSelector('canvas');
  // `__lpHeapStats` exists only in a `--features heap-stats` build.
  const heap = () => page.evaluate(() => {
    const mb = b => Math.round(b / 1048576);
    const m = window.__lpMemory;
    const [live, peak] = window.__lpHeapStats ? window.__lpHeapStats() : [null, null];
    return {
      heapMB: m ? mb(m.buffer.byteLength) : null,
      liveMB: live === null ? null : mb(live),
      peakLiveMB: peak === null ? null : mb(peak),
    };
  });

  const names = await page.evaluate(async () => {
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
    return names;
  });
  const canvas = page.locator('canvas');
  await canvas.click({ position: { x: 800, y: 900 } });
  await page.keyboard.press('Meta+O');
  await page.waitForTimeout(4000);
  await page.keyboard.press('Meta+A');
  await page.keyboard.press('Meta+Shift+E');
  await page.waitForTimeout(500);
  const heapBeforeExport = await heap();
  const t0 = Date.now();
  await page.keyboard.press('Enter');
  const exported = await page.evaluate(async (want) => {
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
        if (out.length >= want) return out;
      } catch (e) {}
      await new Promise(r => setTimeout(r, 500));
    }
    return [];
  }, names.length);
  return { exported, ms: Date.now() - t0, heapBeforeExport, heapAfterExport: await heap() };
}
