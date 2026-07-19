// +-------------------------------------------------------------------------
//
//   taskmgr-rs - CPU 诊断页布局计算
//
//   文件:       src/cpu_layout.rs
//
//   日期:       2026年07月19日
//   作者:       OpenAI Codex
// --------------------------------------------------------------------------

//! Pure responsive layout for the CPU diagnostics page.
//!
//! The details region is bottom-anchored and never overlaps the graph. Width determines whether
//! the four diagnostic groups use one row or a 2-by-2 arrangement; the threshold is derived from
//! measured font/control metrics supplied by the page.

use windows_sys::Win32::Foundation::RECT;

use crate::resource::{CPU_DETAIL_GROUP_COUNT, CPU_DETAIL_METRIC_COUNTS};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuLayoutMetrics {
    pub(crate) margin_x: i32,
    pub(crate) margin_y: i32,
    pub(crate) gap: i32,
    pub(crate) title_width: i32,
    pub(crate) title_height: i32,
    pub(crate) status_height: i32,
    pub(crate) metric_row_height: i32,
    pub(crate) group_top_padding: i32,
    pub(crate) group_bottom_padding: i32,
    pub(crate) minimum_group_width: i32,
    pub(crate) metric_label_width: i32,
    pub(crate) minimum_graph_height: i32,
}

impl CpuLayoutMetrics {
    pub(crate) fn normalized(self) -> Self {
        Self {
            margin_x: self.margin_x.max(1),
            margin_y: self.margin_y.max(1),
            gap: self.gap.max(1),
            title_width: self.title_width.max(1),
            title_height: self.title_height.max(1),
            status_height: self.status_height.max(1),
            metric_row_height: self.metric_row_height.max(1),
            group_top_padding: self.group_top_padding.max(1),
            group_bottom_padding: self.group_bottom_padding.max(1),
            minimum_group_width: self.minimum_group_width.max(1),
            metric_label_width: self.metric_label_width.max(1),
            minimum_graph_height: self.minimum_graph_height.max(1),
        }
    }
}

#[derive(Clone)]
pub(crate) struct CpuLayoutPlan {
    pub(crate) title: RECT,
    pub(crate) model: RECT,
    pub(crate) status: RECT,
    pub(crate) graph: RECT,
    pub(crate) groups: [RECT; CPU_DETAIL_GROUP_COUNT],
    pub(crate) metric_labels: [Vec<RECT>; CPU_DETAIL_GROUP_COUNT],
    pub(crate) metric_values: [Vec<RECT>; CPU_DETAIL_GROUP_COUNT],
    pub(crate) four_columns: bool,
    pub(crate) minimum_content_height: i32,
}

pub(crate) fn compute_cpu_layout(client: RECT, metrics: CpuLayoutMetrics) -> CpuLayoutPlan {
    let metrics = metrics.normalized();
    let left = client.left.saturating_add(metrics.margin_x);
    let right = client.right.saturating_sub(metrics.margin_x).max(left);
    let top = client.top.saturating_add(metrics.margin_y);
    let bottom = client.bottom.saturating_sub(metrics.margin_y).max(top);
    let content_width = (right - left).max(0);

    let four_column_minimum = metrics
        .minimum_group_width
        .saturating_mul(CPU_DETAIL_GROUP_COUNT as i32)
        .saturating_add(
            metrics
                .gap
                .saturating_mul((CPU_DETAIL_GROUP_COUNT - 1) as i32),
        );
    let four_columns = content_width >= four_column_minimum;
    let detail_row_count = if four_columns { 1 } else { 2 };
    let metric_rows = CPU_DETAIL_METRIC_COUNTS
        .iter()
        .copied()
        .max()
        .unwrap_or(0)
        .div_ceil(2) as i32;
    let group_height = metrics
        .group_top_padding
        .saturating_add(metric_rows.saturating_mul(metrics.metric_row_height))
        .saturating_add(metrics.group_bottom_padding);
    let details_height = group_height
        .saturating_mul(detail_row_count)
        .saturating_add(metrics.gap.saturating_mul(detail_row_count - 1));
    let title_width = metrics
        .title_width
        .clamp(1, content_width.saturating_div(3).max(1));
    let title = rect(
        left,
        top,
        left.saturating_add(title_width),
        top + metrics.title_height,
    );
    let model = rect(
        title.right.saturating_add(metrics.gap),
        top,
        right,
        top + metrics.title_height,
    );
    let status_top = top
        .saturating_add(metrics.title_height)
        .saturating_add(metrics.gap);
    let status = rect(left, status_top, right, status_top + metrics.status_height);
    let graph_top = status.bottom.saturating_add(metrics.gap);
    let minimum_details_top = graph_top
        .saturating_add(metrics.minimum_graph_height)
        .saturating_add(metrics.gap);
    let details_top = bottom
        .saturating_sub(details_height)
        .max(minimum_details_top);
    let graph_bottom = details_top.saturating_sub(metrics.gap).max(graph_top);
    let graph = rect(left, graph_top, right, graph_bottom);

    let column_count = if four_columns { 4 } else { 2 };
    let available_columns_width =
        content_width.saturating_sub(metrics.gap.saturating_mul(column_count - 1));
    let column_width = available_columns_width.saturating_div(column_count).max(0);
    let groups = std::array::from_fn(|index| {
        let column = if four_columns { index } else { index % 2 } as i32;
        let row = if four_columns { 0 } else { index / 2 } as i32;
        let group_left =
            left.saturating_add(column.saturating_mul(column_width.saturating_add(metrics.gap)));
        let group_right = if column == column_count - 1 {
            right
        } else {
            group_left.saturating_add(column_width)
        };
        let group_top = details_top
            .saturating_add(row.saturating_mul(group_height.saturating_add(metrics.gap)));
        rect(group_left, group_top, group_right, group_top + group_height)
    });

    let metric_labels = std::array::from_fn(|group| {
        metric_rectangles(
            groups[group],
            CPU_DETAIL_METRIC_COUNTS[group],
            metrics,
            true,
        )
    });
    let metric_values = std::array::from_fn(|group| {
        metric_rectangles(
            groups[group],
            CPU_DETAIL_METRIC_COUNTS[group],
            metrics,
            false,
        )
    });
    let minimum_content_height = metrics
        .margin_y
        .saturating_mul(2)
        .saturating_add(metrics.title_height)
        .saturating_add(metrics.status_height)
        .saturating_add(metrics.minimum_graph_height)
        .saturating_add(details_height)
        .saturating_add(metrics.gap.saturating_mul(3));

    CpuLayoutPlan {
        title,
        model,
        status,
        graph,
        groups,
        metric_labels,
        metric_values,
        four_columns,
        minimum_content_height,
    }
}

fn metric_rectangles(
    group: RECT,
    metric_count: usize,
    metrics: CpuLayoutMetrics,
    labels: bool,
) -> Vec<RECT> {
    let inner_left = group.left.saturating_add(metrics.gap);
    let inner_right = group.right.saturating_sub(metrics.gap).max(inner_left);
    let inner_width = (inner_right - inner_left).max(0);
    let pair_width = inner_width
        .saturating_sub(metrics.gap)
        .saturating_div(2)
        .max(0);
    let label_width = metrics
        .metric_label_width
        .min(pair_width.saturating_mul(55).saturating_div(100))
        .max(0);
    (0..metric_count)
        .map(|index| {
            let pair = (index % 2) as i32;
            let row = (index / 2) as i32;
            let pair_left = inner_left
                .saturating_add(pair.saturating_mul(pair_width.saturating_add(metrics.gap)));
            let pair_right = if pair == 1 {
                inner_right
            } else {
                pair_left.saturating_add(pair_width)
            };
            let row_top = group
                .top
                .saturating_add(metrics.group_top_padding)
                .saturating_add(row.saturating_mul(metrics.metric_row_height));
            if labels {
                rect(
                    pair_left,
                    row_top,
                    pair_left.saturating_add(label_width),
                    row_top.saturating_add(metrics.metric_row_height),
                )
            } else {
                rect(
                    pair_left.saturating_add(label_width),
                    row_top,
                    pair_right,
                    row_top.saturating_add(metrics.metric_row_height),
                )
            }
        })
        .collect()
}

fn rect(left: i32, top: i32, right: i32, bottom: i32) -> RECT {
    RECT {
        left,
        top,
        right: right.max(left),
        bottom: bottom.max(top),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metrics() -> CpuLayoutMetrics {
        CpuLayoutMetrics {
            margin_x: 8,
            margin_y: 8,
            gap: 6,
            title_width: 52,
            title_height: 22,
            status_height: 16,
            metric_row_height: 18,
            group_top_padding: 18,
            group_bottom_padding: 6,
            minimum_group_width: 220,
            metric_label_width: 75,
            minimum_graph_height: 96,
        }
    }

    #[test]
    fn wide_layout_uses_four_columns_and_keeps_graph_above_details() {
        let layout = compute_cpu_layout(
            RECT {
                left: 0,
                top: 0,
                right: 1200,
                bottom: 720,
            },
            metrics(),
        );
        assert!(layout.four_columns);
        assert!(layout.graph.bottom <= layout.groups[0].top);
        assert!(
            layout
                .groups
                .windows(2)
                .all(|pair| pair[0].right <= pair[1].left)
        );
    }

    #[test]
    fn narrow_layout_uses_two_by_two_groups_without_overlap() {
        let layout = compute_cpu_layout(
            RECT {
                left: 0,
                top: 0,
                right: 700,
                bottom: 720,
            },
            metrics(),
        );
        assert!(!layout.four_columns);
        assert!(layout.groups[0].right <= layout.groups[1].left);
        assert!(layout.groups[0].bottom <= layout.groups[2].top);
        assert!(layout.graph.bottom <= layout.groups[0].top);
    }

    #[test]
    fn undersized_client_never_overlaps_graph_and_detail_region() {
        let layout = compute_cpu_layout(
            RECT {
                left: 0,
                top: 0,
                right: 400,
                bottom: 180,
            },
            metrics(),
        );
        assert!(layout.graph.bottom <= layout.groups[0].top);
        assert!(layout.graph.bottom >= layout.graph.top);
    }
}
