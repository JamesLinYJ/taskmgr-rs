// +-------------------------------------------------------------------------
//
//   taskmgr-rs - GPU 页面布局计算
//
//   文件:       src/pages/gpu/layout.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Computes complete GPU-page layouts without mutating Win32 controls.
//!
//! The UI layer commits one `GpuLayoutPlan` with `DeferWindowPos`. Keeping geometry here makes
//! resize behavior deterministic and lets tests prove ordering, bounds and scroll invariants.

use windows_sys::Win32::Foundation::RECT;

pub(crate) const ENGINE_SLOT_COUNT: usize = 4;
pub(crate) const DETAIL_ROW_COUNT: usize = 5;

const EMPTY_RECT: RECT = RECT {
    left: 0,
    top: 0,
    right: 0,
    bottom: 0,
};

#[derive(Clone, Copy)]
pub(crate) struct GpuLayoutMetrics {
    base_x: i32,
    base_y: i32,
    text_line_height: i32,
    combo_visible_height: i32,
}

impl GpuLayoutMetrics {
    pub(crate) fn new(
        base_x: i32,
        base_y: i32,
        text_line_height: i32,
        combo_visible_height: i32,
    ) -> Self {
        Self {
            base_x: base_x.max(1),
            base_y: base_y.max(1),
            text_line_height: text_line_height.max(1),
            combo_visible_height: combo_visible_height.max(1),
        }
    }
}

pub(crate) struct GpuLayoutPlan {
    pub(crate) selector: RECT,
    pub(crate) model: RECT,
    pub(crate) status: RECT,
    pub(crate) engine_selectors: [RECT; ENGINE_SLOT_COUNT],
    pub(crate) engine_percentages: [RECT; ENGINE_SLOT_COUNT],
    pub(crate) engine_graphs: [RECT; ENGINE_SLOT_COUNT],
    pub(crate) dedicated_caption: RECT,
    pub(crate) dedicated_graph: RECT,
    pub(crate) shared_caption: RECT,
    pub(crate) shared_graph: RECT,
    pub(crate) metrics_group: RECT,
    pub(crate) details_group: RECT,
    pub(crate) metric_labels: [RECT; DETAIL_ROW_COUNT],
    pub(crate) metric_values: [RECT; DETAIL_ROW_COUNT],
    pub(crate) detail_labels: [RECT; DETAIL_ROW_COUNT],
    pub(crate) detail_values: [RECT; DETAIL_ROW_COUNT],
    pub(crate) content_height: i32,
    pub(crate) scroll_position: i32,
    pub(crate) needs_scrollbar: bool,
}

pub(crate) fn compute_gpu_layout(
    client_rect: RECT,
    metrics: GpuLayoutMetrics,
    requested_scroll_position: i32,
) -> GpuLayoutPlan {
    let width = rect_width(client_rect).max(0);
    let available_height = rect_height(client_rect).max(0);
    let margin = (metrics.base_x * 2).max(6);
    let gap = margin;
    let header_height = (metrics.base_y * 3)
        .max(22)
        .max(metrics.combo_visible_height);
    let status_height = (metrics.base_y * 2).max(16).max(metrics.text_line_height);
    let graph_caption_height = header_height;
    let graph_caption_gap = metrics.base_y.max(2);
    let caption_text_top_inset = ((graph_caption_height - metrics.text_line_height) / 2).max(0);
    let preferred_engine_height = (metrics.base_y * 10).max(76);
    let preferred_memory_height = (metrics.base_y * 8).max(58);
    let minimum_graph_height = (metrics.base_y * 3).max(24);
    let details_height = (metrics.base_y * 17).max(136);
    let chrome_height = margin
        + header_height
        + status_height
        + gap
        + graph_caption_height * 2
        + graph_caption_gap * 2
        + gap * 2
        + (status_height + graph_caption_gap + gap) * 2
        + details_height
        + margin;
    let preferred_graph_heights = [
        preferred_engine_height,
        preferred_engine_height,
        preferred_memory_height,
        preferred_memory_height,
    ];
    let minimum_graph_heights = [minimum_graph_height; 4];
    let available_graph_height = (available_height - chrome_height).max(0);
    let minimum_graph_total = minimum_graph_heights.iter().sum::<i32>();
    let graph_heights = fit_graph_heights(
        preferred_graph_heights,
        minimum_graph_heights,
        available_graph_height.max(minimum_graph_total),
    );
    let content_height = chrome_height + graph_heights.iter().sum::<i32>();
    let max_scroll = (content_height - available_height).max(0);
    let scroll_position = requested_scroll_position.clamp(0, max_scroll);

    let content_width = (width - margin * 2).max(0);
    let column_gap = gap.min(content_width);
    let column_width = ((content_width - column_gap) / 2).max(0);
    let second_column = margin + column_width + column_gap;
    let mut y = margin - scroll_position;

    let selector_room = (content_width - column_gap).max(0);
    let selector_width = if selector_room == 0 {
        content_width
    } else {
        (content_width * 2 / 5).max(120).min(selector_room)
    };
    let model_gap = if selector_width < content_width {
        column_gap
    } else {
        0
    };
    let model_left = margin + selector_width + model_gap;
    let selector = rect_xywh(margin, y, selector_width, header_height * 8);
    let model = rect_xywh(
        model_left,
        y + caption_text_top_inset,
        (margin + content_width - model_left).max(0),
        metrics.text_line_height,
    );
    y += header_height;
    let status = rect_xywh(margin, y, content_width, status_height);
    y += status_height + gap;

    let mut engine_selectors = [EMPTY_RECT; ENGINE_SLOT_COUNT];
    let mut engine_percentages = [EMPTY_RECT; ENGINE_SLOT_COUNT];
    let mut engine_graphs = [EMPTY_RECT; ENGINE_SLOT_COUNT];
    for (row, &engine_height) in graph_heights.iter().take(2).enumerate() {
        for column in 0..2 {
            let slot = row * 2 + column;
            let x = if column == 0 { margin } else { second_column };
            let combo_gap = gap.min(column_width);
            let combo_width = (column_width * 3 / 4)
                .min((column_width - combo_gap).max(0))
                .max(0);
            engine_selectors[slot] = rect_xywh(x, y, combo_width, graph_caption_height * 8);
            engine_percentages[slot] = rect_xywh(
                x + combo_width + combo_gap,
                y + caption_text_top_inset,
                (column_width - combo_width - combo_gap).max(0),
                metrics.text_line_height,
            );
            engine_graphs[slot] = rect_xywh(
                x,
                y + graph_caption_height + graph_caption_gap,
                column_width,
                engine_height,
            );
        }
        y += graph_caption_height + graph_caption_gap + engine_height + gap;
    }

    let dedicated_caption = rect_xywh(margin, y, content_width, status_height);
    let dedicated_graph = rect_xywh(
        margin,
        y + status_height + graph_caption_gap,
        content_width,
        graph_heights[2],
    );
    y += status_height + graph_caption_gap + graph_heights[2] + gap;
    let shared_caption = rect_xywh(margin, y, content_width, status_height);
    let shared_graph = rect_xywh(
        margin,
        y + status_height + graph_caption_gap,
        content_width,
        graph_heights[3],
    );
    y += status_height + graph_caption_gap + graph_heights[3] + gap;

    let metrics_group = rect_xywh(margin, y, column_width, details_height);
    let details_group = rect_xywh(second_column, y, column_width, details_height);
    let inner_margin = margin.min(column_width / 2);
    let inner_width = (column_width - inner_margin * 2).max(0);
    let row_height = ((details_height - inner_margin * 2) / 6).max(metrics.base_y * 2);
    let label_width = inner_width * 47 / 100;
    let value_width = (inner_width - label_width).max(0);
    let mut metric_labels = [EMPTY_RECT; DETAIL_ROW_COUNT];
    let mut metric_values = [EMPTY_RECT; DETAIL_ROW_COUNT];
    let mut detail_labels = [EMPTY_RECT; DETAIL_ROW_COUNT];
    let mut detail_values = [EMPTY_RECT; DETAIL_ROW_COUNT];
    for index in 0..DETAIL_ROW_COUNT {
        let row_y = y + inner_margin + row_height * (index as i32 + 1);
        metric_labels[index] = rect_xywh(margin + inner_margin, row_y, label_width, row_height);
        metric_values[index] = rect_xywh(
            margin + inner_margin + label_width,
            row_y,
            value_width,
            row_height,
        );
        detail_labels[index] =
            rect_xywh(second_column + inner_margin, row_y, label_width, row_height);
        detail_values[index] = rect_xywh(
            second_column + inner_margin + label_width,
            row_y,
            value_width,
            row_height,
        );
    }

    GpuLayoutPlan {
        selector,
        model,
        status,
        engine_selectors,
        engine_percentages,
        engine_graphs,
        dedicated_caption,
        dedicated_graph,
        shared_caption,
        shared_graph,
        metrics_group,
        details_group,
        metric_labels,
        metric_values,
        detail_labels,
        detail_values,
        content_height,
        scroll_position,
        needs_scrollbar: content_height > available_height,
    }
}

fn fit_graph_heights(preferred: [i32; 4], minimum: [i32; 4], target_total: i32) -> [i32; 4] {
    let preferred_total = preferred.iter().sum::<i32>();
    let minimum_total = minimum.iter().sum::<i32>();
    let target_total = target_total.max(minimum_total);
    let mut result = preferred;

    if target_total >= preferred_total {
        let extra = target_total - preferred_total;
        let per_graph = extra / result.len() as i32;
        let remainder = extra % result.len() as i32;
        for (index, height) in result.iter_mut().enumerate() {
            *height += per_graph + i32::from((index as i32) < remainder);
        }
        return result;
    }

    let mut remaining = preferred_total - target_total;
    while remaining > 0 {
        let active = result
            .iter()
            .zip(minimum)
            .filter(|(height, minimum)| **height > *minimum)
            .count();
        if active == 0 {
            break;
        }
        let active = active as i32;
        let share = ((remaining + active - 1) / active).max(1);
        for (height, minimum) in result.iter_mut().zip(minimum) {
            let reduction = (*height - minimum).min(share).min(remaining);
            *height -= reduction;
            remaining -= reduction;
            if remaining == 0 {
                break;
            }
        }
    }
    result
}

fn rect_xywh(x: i32, y: i32, width: i32, height: i32) -> RECT {
    RECT {
        left: x,
        top: y,
        right: x + width.max(0),
        bottom: y + height.max(0),
    }
}

fn rect_width(rect: RECT) -> i32 {
    rect.right - rect.left
}

fn rect_height(rect: RECT) -> i32 {
    rect.bottom - rect.top
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client(width: i32, height: i32) -> RECT {
        RECT {
            left: 0,
            top: 0,
            right: width,
            bottom: height,
        }
    }

    fn metrics() -> GpuLayoutMetrics {
        GpuLayoutMetrics::new(2, 2, 18, 28)
    }

    #[test]
    fn wide_layout_keeps_columns_and_graph_sections_separate() {
        let layout = compute_gpu_layout(client(1200, 900), metrics(), 0);
        assert!(!layout.needs_scrollbar);
        assert!(layout.engine_graphs[0].right < layout.engine_graphs[1].left);
        assert_eq!(layout.engine_graphs[0].top, layout.engine_graphs[1].top);
        assert!(layout.dedicated_graph.top >= layout.engine_graphs[2].bottom);
        assert!(layout.shared_graph.top >= layout.dedicated_graph.bottom);
        assert!(layout.metrics_group.top >= layout.shared_graph.bottom);
        assert!(layout.details_group.left > layout.metrics_group.right);
        assert!(layout.shared_graph.right <= 1200);
    }

    #[test]
    fn short_layout_clamps_scroll_position_to_content() {
        let first = compute_gpu_layout(client(700, 320), metrics(), i32::MAX);
        assert!(first.needs_scrollbar);
        assert!(first.content_height > 320);
        assert_eq!(first.scroll_position, first.content_height - 320);
        assert_eq!(first.details_group.bottom, 320 - 6);

        let top = compute_gpu_layout(client(700, 320), metrics(), -100);
        assert_eq!(top.scroll_position, 0);
        assert_eq!(top.selector.top, 6);
    }

    #[test]
    fn resizing_recomputes_full_width_graph_bounds() {
        let narrow = compute_gpu_layout(client(640, 900), metrics(), 0);
        let wide = compute_gpu_layout(client(1000, 900), metrics(), 0);
        assert_eq!(narrow.shared_graph.right, 640 - 6);
        assert_eq!(wide.shared_graph.right, 1000 - 6);
        assert!(rect_width(wide.shared_graph) > rect_width(narrow.shared_graph));
    }

    #[test]
    fn compressed_layout_keeps_details_at_bottom_and_shrinks_graphs() {
        let tall = compute_gpu_layout(client(1000, 900), metrics(), 0);
        let compressed = compute_gpu_layout(client(1000, 500), metrics(), 0);

        assert!(!compressed.needs_scrollbar);
        assert_eq!(compressed.details_group.bottom, 500 - 6);
        assert_eq!(compressed.metrics_group.bottom, 500 - 6);
        assert!(rect_height(compressed.engine_graphs[0]) < rect_height(tall.engine_graphs[0]));
        assert!(rect_height(compressed.shared_graph) < rect_height(tall.shared_graph));
        assert!(rect_height(compressed.engine_graphs[0]) >= 24);
        assert!(rect_height(compressed.shared_graph) >= 24);
    }

    #[test]
    fn graph_height_fitting_preserves_exact_target_and_minimums() {
        let fitted = fit_graph_heights([76, 76, 58, 58], [24; 4], 208);

        assert_eq!(fitted.iter().sum::<i32>(), 208);
        assert!(fitted.into_iter().all(|height| height >= 24));
    }

    #[test]
    fn caption_controls_end_before_their_graphs_begin() {
        let layout = compute_gpu_layout(client(1000, 500), metrics(), 0);

        for slot in 0..ENGINE_SLOT_COUNT {
            assert!(
                layout.engine_selectors[slot].top + metrics().combo_visible_height
                    < layout.engine_graphs[slot].top
            );
            assert!(layout.engine_percentages[slot].bottom < layout.engine_graphs[slot].top);
        }
        assert!(layout.dedicated_caption.bottom < layout.dedicated_graph.top);
        assert!(layout.shared_caption.bottom < layout.shared_graph.top);
    }
}
