# System diagram

This page has two diagrams of LightPhotos. The component diagram shows the modules and
the connections between them. The sequence diagram shows the path of one photo from a
click to the screen.

## Components

The app binary is `src/main.rs`, `src/app/` and `src/ui/`. It uses the lib, `src/lib.rs`.
The lib does not use egui, winit or wgpu.

A dashed border shows code that builds on one platform only:

- Blue: macOS.
- Orange: Linux, Windows and the browser.
- Green: the browser.

```mermaid
flowchart LR
    user([User])

    subgraph shell["Platform shell"]
        main["main.rs<br/>winit event loop"]
        delegate["macos_delegate.rs<br/>Finder open-document"]
        menu["menu.rs<br/>native menu bar"]
        drive["drive.rs<br/>--drive headless scripts"]
        profile["profile.rs<br/>--profile (hotpath)"]
        webshell["src/web/*<br/>web_fs, web_canvas, analytics"]
    end

    subgraph appg["App coordinator (src/app/)"]
        app["App (mod.rs)<br/>per-frame pump"]
        appmods["nav · keys · loupe · thumbs · adjust · crop<br/>autotone · score · faces · bursts · group_compare<br/>export · presets · histogram · session · web"]
    end

    subgraph uig["Drawing"]
        ui["src/ui/*<br/>egui chrome: grid, filmstrip,<br/>toolbar, panels, form.rs"]
        renderer["renderer.rs + shader.wgsl<br/>wgpu loupe image"]
        rawrender["raw/render.rs<br/>linear RAW tonemap (browser Loupe)"]
    end

    subgraph bg["Background work"]
        loader["loader.rs<br/>priority queue, reserved worker<br/>preview/full LRU + thumb LRU"]
        thumbs["thumbnail.rs<br/>ThumbCache on disk"]
        wpool["worker_pool.rs<br/>export jobs"]
        spool["score.rs<br/>ScorePool, 2-4 workers"]
        fpool["facequality.rs<br/>FacePool, 2 workers"]
        seg["segmentation.rs<br/>one thread per request"]
    end

    subgraph lib["Lib (src/lib.rs)"]
        decode["image_decode.rs / image_encode.rs<br/>coregraphics.rs"]
        nonmac["raw/nonmac_decode.rs<br/>raw_preview (Fast / Quality)"]
        develop["develop.rs<br/>Adjustments → GpuAdjust<br/>apply_linear (CPU mirror)"]
        imgops["image_ops.rs<br/>crop / tone / rotate, bake_edited"]
        export["export.rs<br/>ExportJob → ExportDest"]
        immich["immich.rs<br/>HTTP upload client"]
        judge["quality.rs + judge.rs<br/>technical score × penalty curves"]
        vision["vision.rs<br/>VNRequest plumbing"]
    end

    subgraph persist["Persistence"]
        catalog["catalog.rs<br/>ratings, flags, labels, edits"]
        writeback["catalog/writeback.rs<br/>off-frame write queue"]
        groups["groups.rs<br/>photo groups / bursts"]
        signal["signalcache.rs<br/>capture time, face quality"]
        prefs["prefs.rs · presets.rs"]
        secret["secret.rs<br/>Immich API key"]
    end

    subgraph ext["Outside the process"]
        folder[("Photo folder<br/>originals, never written")]
        sidecars[(".lightphotos/<br/>*.xmp JSON sidecars<br/>groups/*.json")]
        imageio["Apple ImageIO /<br/>CoreGraphics"]
        applevision["Apple Vision"]
        crates["image · rawler · mozjpeg-rs<br/>kamadak-exif"]
        fsa["Browser File System<br/>Access API"]
        gpu["GPU (wgpu)"]
        immichsrv["Immich server"]
        keychain["macOS Keychain"]
    end

    user --> main
    user -. "double-click in Finder" .-> delegate --> main
    menu --> app
    main -- "window_event / key_input" --> app
    drive -- "synthetic events" --> app
    profile -- "headless phases" --> loader
    webshell --> app
    webshell --> fsa

    app --- appmods
    app -- "builds frame" --> ui
    ui -- "UiAction, image rect" --> app
    app -- "set_image / set_transform<br/>set_adjustments(GpuAdjust)" --> renderer
    renderer --- rawrender
    renderer --> gpu
    ui --> gpu

    app -- "request_preview / request_full<br/>request_thumb / request_bake" --> loader
    loader -- "poll_all (once per frame)" --> app
    loader --> thumbs
    loader --> decode
    thumbs --> decode
    decode --> imageio
    decode --> nonmac --> crates

    app -- "ExportJob" --> wpool --> export
    export --> imgops
    export --> decode
    export -- "ExportDest::Immich" --> immich --> immichsrv
    immich -. "api key" .- secret --> keychain

    app -- "ScoreJob" --> spool --> judge
    spool --> imgops
    judge --> vision
    app --> fpool --> vision
    app --> seg --> vision
    vision --> applevision

    develop --> imgops
    app --> develop

    app -- "set / set_adjustments<br/>apply_group_writes" --> catalog
    catalog --> writeback --> sidecars
    catalog --- groups
    app --> signal --> sidecars
    app --> prefs
    decode -. "reads" .-> folder
    sidecars -. "lives in" .- folder

    classDef mac stroke:#1f6feb,stroke-width:2px,stroke-dasharray:5 3
    classDef nonmac stroke:#d97706,stroke-width:2px,stroke-dasharray:5 3
    classDef web stroke:#16a34a,stroke-width:2px,stroke-dasharray:5 3
    class delegate,menu,vision,spool,imageio,applevision,keychain mac
    class nonmac,crates nonmac
    class webshell,fsa,rawrender web
```

`facequality.rs`, `segmentation.rs` and the aesthetics part of `judge.rs` build on all
platforms. On Linux, Windows and the browser, they return `Err` or a flat base score. Thus,
the diagram does not mark them as macOS code.

`score.rs` builds on all platforms. But `ScorePool::new` returns `None` off macOS, so
scoring runs on macOS only. The diagram marks it as macOS code.

`raw/render.rs` builds on all platforms. But only the browser Loupe RAW path gives it
linear-light images. All other paths apply the tonemap on the CPU.

## Opening a photo

This diagram shows three user actions in sequence:

1. A double-click on a grid cell opens the photo in the Loupe.
2. A scroll or pinch zooms the photo.
3. A number key sets the star rating.

Decode does not occur on the UI thread. A zoom does not cause a decode.

```mermaid
sequenceDiagram
    actor U as User
    participant M as main.rs (winit)
    participant A as App
    participant UI as ui/ (egui)
    participant L as loader.rs
    participant W as Decode worker
    participant D as image_decode
    participant R as renderer.rs
    participant C as Catalog
    participant Q as writeback queue

    U->>M: double-click grid cell
    M->>A: window_event
    A->>UI: run egui frame
    UI-->>A: UiAction::Select(i), open Loupe
    A->>L: request_preview(path, px)
    Note over L: full / preview jobs outrank thumbnails,<br/>one worker is reserved for them
    L->>W: dequeue job
    W->>D: decode at preview size
    D-->>W: DecodedImage
    W-->>L: result on shared channel
    A->>L: poll_all (next frame)
    L-->>A: preview ready
    A->>R: set_image (upload GPU texture)
    A->>R: set_adjustments(GpuAdjust)
    R-->>U: loupe frame
    A->>L: request_full(path) when zoom needs it

    U->>M: scroll / pinch to zoom
    M->>A: window_event
    A->>R: set_transform(scale, offset, rot)
    Note over R: uniform update only, no re-decode
    R-->>U: next frame

    U->>M: key "3"
    M->>A: key_input
    A->>C: set(path, 3 stars)
    C->>Q: enqueue WriteOp
    Note over C,Q: background thread writes<br/>.lightphotos/<photo>.xmp<br/>(temp file + rename)
```
