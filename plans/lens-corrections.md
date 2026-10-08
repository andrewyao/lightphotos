# Lens corrections: findings (2026-10-08)

Input for phase 2 of Optics ("Enable Lens Corrections"). Phase 1, Remove
Chromatic Aberration, is in `src/chroma.rs`.

## What macOS does today

- `CGImageSource` with no options (our RAW decode on macOS) already applies
  Apple's lens correction. On DSC02452 (FE 55mm F1.8 ZA) the default render
  differs from `CIRAWFilter` with lens correction on by 0.47 gray levels, and
  from it off by 1.75.
- Apple corrects only lenses it supports. `CIRAWFilter.isLensCorrectionSupported`
  is true for the FE 55mm F1.8 ZA and false for the FE 85mm F1.8, although both
  files carry Sony's correction data.
- Apple does not apply Sony's embedded curve. On DSC01977 a generic r² curve fits
  Apple's on/off difference better than the embedded knots. Apple seems to use
  its own per-lens profiles.
- So macOS and non-mac renders of the same ARW already differ in geometry for
  supported lenses, and macOS never corrects unsupported ones.

## Sony ARW embedded data

All tags are in the raw SubIFD (IFD0 → 0x14A[0]) as SSHORT arrays. Element 0 is
the knot count.

| Tag | Name | Layout |
|---|---|---|
| 0x7031 | VignettingCorrection | flag |
| 0x7032 | VignettingCorrParams | 16 knots |
| 0x7034 | ChromaticAberrationCorrection | flag |
| 0x7035 | ChromaticAberrationCorrParams | 32: probably 16 red, then 16 blue |
| 0x7036 | DistortionCorrection | flag (17 and 0 seen) |
| 0x7037 | DistortionCorrParams | 16 knots |

The knots change from shot to shot with focus distance, so they are per photo,
not per lens.

## Distortion model (verified)

```
c(t)   = linear interpolation of the 16 knots over t = r / half-diagonal, knot i at t = i / 15
scale  = (1 + c(t) * 2^-14) / (1 + max(knots) * 2^-14)
source = center + (p - center) * scale        // for each output pixel p
```

The divisor zooms in so the frame stays full, with no empty corners. This was
checked against the camera's own embedded preview JPEG, which the camera
corrected, with no free parameters. 1 − correlation of high-passed gray:

| File | Lens | No warp | Model |
|---|---|---|---|
| DSC02452 | FE 55mm F1.8 ZA | 0.386 | 0.038 |
| DSC01977 | FE 55mm F1.8 ZA | 0.554 | 0.020 |
| DSC02009 | FE 85mm F1.8 (Apple: unsupported) | 0.156 | 0.044 |

A free fit on DSC02452 lands on a gain of 2^-14.00 and an x scale of 0.9963,
against the 0.9965 the model predicts.

## Not verified yet

- The vignetting and CA encodings. Fit them against the preview JPEG the same
  way: vignetting from the brightness ratio by radius, CA by a per-channel
  radial scale.
- Other makers: Fuji RAF, Micro Four Thirds, DNG opcodes.
- What to do on macOS. Either keep Apple's correction where it exists and add
  Sony's elsewhere, or turn Apple's off (`CIRAWFilter`, which needs a
  CoreImage binding) and apply Sony's everywhere for parity.

The scripts that produced these numbers (`compare.swift`, `fitcam.swift`,
`fixcam.swift`, `sony_tags.py`) are throwaway and not committed.
