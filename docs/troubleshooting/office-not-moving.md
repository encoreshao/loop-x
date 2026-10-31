# The Office view is frozen: nobody walks, the cars and pedestrians stand still

## Symptom

In the Office view the colleagues stay in their seats and never get up or talk,
and the cars, pedestrians, clouds and birds outside do not move. The day/night
colours and the colleagues' states (working, waiting, failed) still update.

## Cause

macOS **Reduce Motion** is switched on. The Office reads it through the page's
`prefers-reduced-motion` setting and, by design, draws everything still: no
walking, talking, traffic, clouds or birds. It is an accessibility feature, not
a bug, and it applies to any browser and to the LoopX app.

## Check

```bash
defaults read com.apple.universalaccess reduceMotion
```

`1` means Reduce Motion is on, `0` that it is off.

## Fix

Either turn the system setting off, or tell the Office to animate anyway.

**Turn Reduce Motion off (the whole Mac):**

1. Open **System Settings → Accessibility → Motion**. On some macOS versions
   it is under **Accessibility → Display** instead.
2. Switch **Reduce motion** off.
3. Reload the Office view, or reopen the LoopX app.

**Animate only the Office (leave Reduce Motion on):**

When Reduce Motion is on, the Office shows an **Animate the office** button at
the bottom left. Press it; the page reloads and everything moves. Press it
again to go back to a still office. The choice is remembered per browser (or
per LoopX app) in local storage, so it does not change any system setting.

## Still frozen?

- Reload the page after changing a setting; the Office reads it once, when it
  loads.
- After updating LoopX, quit and reopen the app so it serves the new version.
- If the Office shows the List view with a WebGL note instead of the 3D scene,
  that is a different problem: the browser has WebGL turned off.
