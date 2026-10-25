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

  const N = 120;
  const photos = await page.evaluate(async (N) => {
    const listing = await (await fetch('/bench/')).text();
    const names = [...listing.matchAll(/href="([^"]+\.ARW)"/gi)].map(m => decodeURIComponent(m[1])).slice(0, N);
    const opfs = await navigator.storage.getDirectory();
    try { await opfs.removeEntry('raw', { recursive: true }); } catch (e) {}
    const root = await opfs.getDirectoryHandle('raw', { create: true });
    let i = 0;
    await Promise.all(Array.from({ length: 6 }, async () => {
      while (i < names.length) {
        const name = names[i++];
        const bytes = await (await fetch('/bench/' + encodeURIComponent(name))).arrayBuffer();
        const w = await (await root.getFileHandle(name, { create: true })).createWritable();
        await w.write(bytes); await w.close();
      }
    }));
    window.__lpTestRoot = root;
    window.__long = [];
    new PerformanceObserver(l => { for (const e of l.getEntries()) window.__long.push(e.duration); })
      .observe({ type: 'longtask', buffered: false });
    return names.length;
  }, N);

  // Time until the screen stops changing: three identical screenshots 150 ms
  // apart, measured from `t0`.
  const settle = async (t0, limitMs = 120000) => {
    let prev = null, same = 0;
    while (Date.now() - t0 < limitMs) {
      const shot = await page.screenshot({ type: 'jpeg', quality: 40, scale: 'css' });
      if (prev && shot.equals(prev)) {
        if (++same >= 3) return Date.now() - t0 - 3 * 150;
      } else {
        same = 0;
      }
      prev = shot;
      await page.waitForTimeout(150);
    }
    return null;
  };
  const longMs = () => page.evaluate(() => {
    const l = window.__long; window.__long = [];
    return { longtasks: l.length, longtaskMs: Math.round(l.reduce((a, b) => a + b, 0)) };
  });

  const canvas = page.locator('canvas');
  await canvas.click({ position: { x: 800, y: 900 } });
  let t0 = Date.now();
  await page.keyboard.press('Meta+O');
  const openMs = await settle(t0);
  const open = await longMs();
  const heapAfterOpen = await heap();

  await page.mouse.move(800, 900);
  t0 = Date.now();
  for (let k = 0; k < 20; k++) {
    await page.mouse.wheel(0, 500);
    await page.waitForTimeout(16);
  }
  const flickMs = await settle(t0);
  const flick = await longMs();
  const heapAfterFlick = await heap();

  // Flick back to the top and open a photo at once, while the flick's
  // thumbnails are still queued: the Loupe has to get ahead of them.
  t0 = Date.now();
  for (let k = 0; k < 20; k++) {
    await page.mouse.wheel(0, -500);
    await page.waitForTimeout(16);
  }
  await canvas.click({ position: { x: 500, y: 400 } });
  await page.keyboard.press('e');
  const loupeMs = await settle(t0);
  const loupe = await longMs();

  return {
    photos, openMs, open, flickMs, flick, loupeMs, loupe,
    heapAfterOpen, heapAfterFlick, heapAfterLoupe: await heap(),
  };
}
