//! 性能页布局纯逻辑模块。
//! 这里不直接调用 Win32 重排控件，只负责根据页面矩形、锚点和间距计算出目标布局，
//! 让性能页消息层保持更薄，也让后续调布局时更容易验证。

use windows_sys::Win32::Foundation::RECT;

#[derive(Clone, Copy)]
pub struct PerfDialogSpacing {
    // 对话框单位换算后的页面级间距配置。
    pub def_spacing: i32,
    pub inner_spacing: i32,
    pub top_spacing: i32,
}

#[derive(Clone, Copy)]
pub struct PerfLayoutAnchors {
    // 这些锚点来自运行时对话框模板里的初始控件位置，用来保持经典布局比例。
    pub master_rect: RECT,
    pub top_frame: RECT,
    pub cpu_history_frame: RECT,
    pub cpu_usage_frame: RECT,
    pub mem_bar_frame: RECT,
    pub mem_frame: RECT,
}

pub struct PerfLayoutPlan {
    // `PerfLayoutPlan` 是一次布局计算的结果快照，页面层只需要按这里的结果提交。
    pub detail_shift_y: i32,
    pub cpu_history_width: i32,
    pub cpu_history_height: i32,
    pub cpu_usage_frame_width: i32,
    pub meter_rect: RECT,
    pub mem_bar_frame_rect: RECT,
    pub mem_frame_rect: RECT,
    pub mem_graph_rect: RECT,
    pub cpu_pane_rects: Vec<RECT>,
    pub graph_surface_width: i32,
    pub graph_surface_height: i32,
}

pub fn compute_perf_layout(
    parent_rect: RECT,
    anchors: PerfLayoutAnchors,
    spacing: PerfDialogSpacing,
    pane_count: usize,
    no_title: bool,
) -> PerfLayoutPlan {
    // 所有尺寸和位置都在这里一次性算出，避免消息层边读控件边改布局。
    let detail_shift_y = ((parent_rect.bottom - spacing.def_spacing * 2)
        - anchors.master_rect.bottom)
        .max(-anchors.master_rect.bottom);

    let y_top = anchors.top_frame.top + detail_shift_y;
    let cpu_history_height = if no_title {
        parent_rect.bottom - parent_rect.top - spacing.def_spacing * 2
    } else {
        (y_top - spacing.def_spacing * 3) / 2
    }
    .max(0);

    let cpu_history_width =
        (parent_rect.right - anchors.cpu_history_frame.left - spacing.def_spacing * 2).max(0);
    let cpu_usage_frame_width = rect_width(anchors.cpu_usage_frame).max(0);
    let graph_height =
        (cpu_history_height - spacing.inner_spacing * 2 - spacing.top_spacing).max(0);

    let meter_left = anchors.cpu_usage_frame.left + spacing.inner_spacing * 2;
    let meter_top = anchors.cpu_usage_frame.top + spacing.top_spacing;
    let meter_rect = RECT {
        left: meter_left,
        top: meter_top,
        right: (anchors.cpu_usage_frame.right - spacing.inner_spacing * 2).max(meter_left),
        bottom: meter_top + graph_height,
    };

    let mem_top = cpu_history_height + spacing.def_spacing * 2;
    let mem_bar_frame_rect = RECT {
        left: anchors.mem_bar_frame.left,
        top: mem_top,
        right: anchors.mem_bar_frame.right,
        bottom: mem_top + cpu_history_height,
    };

    let mem_frame_rect = RECT {
        left: anchors.mem_frame.left,
        top: mem_top,
        right: parent_rect.right - spacing.def_spacing * 2,
        bottom: mem_top + cpu_history_height,
    };

    let mem_graph_left = mem_frame_rect.left + spacing.inner_spacing * 2;
    let mem_graph_rect = RECT {
        left: mem_graph_left,
        top: mem_top + spacing.top_spacing,
        right: (mem_frame_rect.right - spacing.inner_spacing * 2).max(mem_graph_left),
        bottom: mem_top + spacing.top_spacing + graph_height,
    };

    let pane_area_left = anchors.cpu_history_frame.left + spacing.inner_spacing * 2;
    let pane_area_top = anchors.cpu_history_frame.top + spacing.top_spacing;
    let pane_area_width =
        (parent_rect.right - spacing.def_spacing * 2 - spacing.inner_spacing * 2 - pane_area_left)
            .max(0);
    let mut cpu_pane_rects = Vec::with_capacity(pane_count);
    if pane_count > 0 {
        let grid = choose_pane_grid(
            pane_count,
            pane_area_width,
            graph_height,
            spacing.inner_spacing,
        );

        for pane_index in 0..pane_count {
            let row = pane_index / grid.columns;
            let column = pane_index % grid.columns;
            let (x, width) =
                partition_extent(pane_area_width, grid.columns, column, spacing.inner_spacing);
            let (y, height) = partition_extent(graph_height, grid.rows, row, spacing.inner_spacing);
            cpu_pane_rects.push(RECT {
                left: pane_area_left + x,
                top: pane_area_top + y,
                right: pane_area_left + x + width,
                bottom: pane_area_top + y + height,
            });
        }
    }

    let graph_surface_width = cpu_pane_rects
        .first()
        .map(|rect| rect_width(*rect))
        .unwrap_or(0)
        .max(rect_width(mem_graph_rect));

    PerfLayoutPlan {
        detail_shift_y,
        cpu_history_width,
        cpu_history_height,
        cpu_usage_frame_width,
        meter_rect,
        mem_bar_frame_rect,
        mem_frame_rect,
        mem_graph_rect,
        cpu_pane_rects,
        graph_surface_width,
        graph_surface_height: graph_height,
    }
}

pub fn next_graph_surface_extent(current: i32, required: i32, quantum: i32) -> i32 {
    // 图表离屏表面按容量而不是按精确像素扩容，
    // 这样慢速拖动窗口时不会因为每增长 1 像素就重建一次位图。
    if current >= required {
        return current;
    }

    let rounded_required = ((required + quantum - 1) / quantum) * quantum;
    let grown_current = if current > 0 {
        current + current / 2
    } else {
        0
    };

    rounded_required.max(grown_current).max(required)
}

fn rect_width(rect: RECT) -> i32 {
    rect.right - rect.left
}

#[derive(Clone, Copy)]
struct PaneGrid {
    columns: usize,
    rows: usize,
}

fn choose_pane_grid(count: usize, width: i32, height: i32, gap: i32) -> PaneGrid {
    let mut best = PaneGrid {
        columns: 1,
        rows: count.max(1),
    };
    let mut best_score = i64::MAX;

    for columns in 1..=count.max(1) {
        let rows = count.div_ceil(columns).max(1);
        let usable_width = width - gap.saturating_mul(columns.saturating_sub(1) as i32);
        let usable_height = height - gap.saturating_mul(rows.saturating_sub(1) as i32);
        if usable_width <= 0 || usable_height <= 0 {
            continue;
        }

        let pane_width = usable_width / columns as i32;
        let pane_height = usable_height / rows as i32;
        if pane_width <= 0 || pane_height <= 0 {
            continue;
        }

        let longer_side = i64::from(pane_width.max(pane_height));
        let aspect_error = i64::from((pane_width - pane_height).abs()) * 1024 / longer_side;
        let unused_slots = columns * rows - count;
        let score = aspect_error + unused_slots as i64 * 32;
        if score < best_score
            || (score == best_score && unused_slots < best.columns * best.rows - count)
        {
            best = PaneGrid { columns, rows };
            best_score = score;
        }
    }

    best
}

fn partition_extent(total: i32, parts: usize, index: usize, gap: i32) -> (i32, i32) {
    let parts = parts.max(1);
    let usable = (total - gap.saturating_mul(parts.saturating_sub(1) as i32)).max(0);
    let base = usable / parts as i32;
    let remainder = usable % parts as i32;
    let index = index.min(parts - 1) as i32;
    let extra_before = index.min(remainder);
    let start = index * (base + gap) + extra_before;
    let size = base + i32::from(index < remainder);
    (start, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
        RECT {
            left,
            top,
            right,
            bottom,
        }
    }

    fn sample_anchors() -> PerfLayoutAnchors {
        PerfLayoutAnchors {
            master_rect: rect(10, 180, 160, 220),
            top_frame: rect(10, 160, 160, 200),
            cpu_history_frame: rect(130, 10, 200, 100),
            cpu_usage_frame: rect(10, 10, 110, 100),
            mem_bar_frame: rect(10, 110, 110, 200),
            mem_frame: rect(130, 110, 200, 200),
        }
    }

    fn sample_spacing() -> PerfDialogSpacing {
        PerfDialogSpacing {
            def_spacing: 6,
            inner_spacing: 3,
            top_spacing: 15,
        }
    }

    #[test]
    fn meter_rect_respects_frame_inner_padding() {
        let layout = compute_perf_layout(
            rect(0, 0, 640, 260),
            sample_anchors(),
            sample_spacing(),
            1,
            false,
        );

        assert_eq!(layout.meter_rect.left, 16);
        assert_eq!(layout.meter_rect.right, 104);
        assert!(layout.meter_rect.right <= sample_anchors().cpu_usage_frame.right);
    }

    #[test]
    fn memory_graph_uses_memory_frame_bounds() {
        let layout = compute_perf_layout(
            rect(0, 0, 640, 260),
            sample_anchors(),
            sample_spacing(),
            1,
            false,
        );

        assert_eq!(layout.mem_graph_rect.left, layout.mem_frame_rect.left + 6);
        assert_eq!(layout.mem_graph_rect.right, layout.mem_frame_rect.right - 6);
    }

    #[test]
    fn many_cpu_panes_form_a_non_overlapping_grid() {
        let layout = compute_perf_layout(
            rect(0, 0, 800, 400),
            sample_anchors(),
            sample_spacing(),
            32,
            false,
        );

        assert_eq!(layout.cpu_pane_rects.len(), 32);
        assert!(
            layout
                .cpu_pane_rects
                .iter()
                .map(|rect| rect.top)
                .collect::<std::collections::HashSet<_>>()
                .len()
                > 1
        );
        for (index, pane) in layout.cpu_pane_rects.iter().enumerate() {
            assert!(pane.right > pane.left);
            assert!(pane.bottom > pane.top);
            for other in &layout.cpu_pane_rects[index + 1..] {
                let overlaps = pane.left < other.right
                    && pane.right > other.left
                    && pane.top < other.bottom
                    && pane.bottom > other.top;
                assert!(!overlaps);
            }
        }
    }
}
