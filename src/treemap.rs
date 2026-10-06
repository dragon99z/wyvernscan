/// Squarified treemap layout (Bruls, Huizing, van Wijk 1999).
/// Produces near-square rectangles, which is what makes WizTree-style
/// treemaps easy to read compared to a naive slice-and-dice layout.
#[derive(Debug, Clone, Copy)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}


/// Lay out `sizes` (already sorted descending by caller for best results)
/// into `bounds`. Returns one `Rect` per input size, same order as input.
/// Zero-size or empty input is handled gracefully.
pub fn squarify(sizes: &[f64], bounds: Rect) -> Vec<Rect> {
    let n = sizes.len();
    let mut out = vec![
        Rect {
            x: bounds.x,
            y: bounds.y,
            w: 0.0,
            h: 0.0
        };
        n
    ];
    if n == 0 || bounds.w <= 0.0 || bounds.h <= 0.0 {
        return out;
    }

    let total: f64 = sizes.iter().sum();
    if total <= 0.0 {
        return out;
    }

    let area_total = (bounds.w as f64) * (bounds.h as f64);
    let scale = area_total / total;

    // Work with scaled areas so the algorithm is purely geometric.
    let areas: Vec<f64> = sizes.iter().map(|s| s * scale).collect();

    let mut remaining = bounds;
    let mut i = 0usize;
    let mut row: Vec<usize> = Vec::new();

    while i < n {
        row.clear();
        row.push(i);
        let side = remaining.w.min(remaining.h) as f64;
        let mut row_worst = worst_ratio(&areas, &row, side);
        let mut j = i + 1;

        while j < n {
            let mut trial = row.clone();
            trial.push(j);
            let trial_worst = worst_ratio(&areas, &trial, side);
            if trial_worst <= row_worst {
                row = trial;
                row_worst = trial_worst;
                j += 1;
            } else {
                break;
            }
        }

        remaining = lay_out_row(&areas, &row, remaining, &mut out);
        i = j;
    }

    out
}

fn worst_ratio(areas: &[f64], row: &[usize], side: f64) -> f64 {
    if side <= 0.0 {
        return f64::INFINITY;
    }
    let sum: f64 = row.iter().map(|&k| areas[k]).sum();
    if sum <= 0.0 {
        return f64::INFINITY;
    }
    let row_len = sum / side; // thickness of the row along the fixed side
    if row_len <= 0.0 {
        return f64::INFINITY;
    }
    let mut worst = 0.0f64;
    for &k in row {
        let a = areas[k];
        let w = a / row_len;
        let ratio = (row_len / w).max(w / row_len);
        if ratio > worst {
            worst = ratio;
        }
    }
    worst
}

/// Place one row of rectangles along the shorter side of `space`, then
/// return the leftover space for the next row.
fn lay_out_row(areas: &[f64], row: &[usize], space: Rect, out: &mut [Rect]) -> Rect {
    let sum: f64 = row.iter().map(|&k| areas[k]).sum();
    if space.w >= space.h {
        // Lay the row out as a vertical strip on the left.
        let strip_w = (sum / space.h as f64) as f32;
        let mut y = space.y;
        for &k in row {
            let h = (areas[k] / strip_w as f64) as f32;
            out[k] = Rect {
                x: space.x,
                y,
                w: strip_w,
                h,
            };
            y += h;
        }
        Rect {
            x: space.x + strip_w,
            y: space.y,
            w: (space.w - strip_w).max(0.0),
            h: space.h,
        }
    } else {
        // Lay the row out as a horizontal strip on top.
        let strip_h = (sum / space.w as f64) as f32;
        let mut x = space.x;
        for &k in row {
            let w = (areas[k] / strip_h as f64) as f32;
            out[k] = Rect {
                x,
                y: space.y,
                w,
                h: strip_h,
            };
            x += w;
        }
        Rect {
            x: space.x,
            y: space.y + strip_h,
            w: space.w,
            h: (space.h - strip_h).max(0.0),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every rectangle's area should be proportional to its input size, and
    /// they should tile the bounds exactly with no gaps or overlaps in area.
    #[test]
    fn areas_are_proportional_to_input_sizes() {
        let sizes = [400.0, 300.0, 200.0, 100.0];
        let bounds = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let rects = squarify(&sizes, bounds);

        let total_area: f32 = rects.iter().map(|r| r.w * r.h).sum();
        assert!((total_area - 100.0 * 100.0).abs() < 1.0);

        // Largest input should get the largest rectangle, proportionally.
        let areas: Vec<f32> = rects.iter().map(|r| r.w * r.h).collect();
        assert!(areas[0] > areas[1] && areas[1] > areas[2] && areas[2] > areas[3]);
    }

    #[test]
    fn empty_input_returns_no_rects() {
        let bounds = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        assert!(squarify(&[], bounds).is_empty());
    }
}
