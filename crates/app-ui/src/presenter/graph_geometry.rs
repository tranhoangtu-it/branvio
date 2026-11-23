//! UI-agnostic Metro commit-graph geometry & HiDPI pixel-snapping calculations.
//!
//! Converts core [`GraphRow`] and [`VirtualWipRow`] descriptors plus [`DisplayScalePolicy`]
//! into deterministic lines, smooth S-curve cubic Béziers, and commit/WIP node circles ready for any canvas backend.

use crate::theme::BranvioTheme;
use branvio_core::domain::{EdgeCurveKind, GraphEdge, GraphNodeKind, GraphRow, VirtualWipRow};
use branvio_core::platform::DisplayScalePolicy;

/// Layout constants controlling Commit Graph row spacing and Metro lane column width.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphLayoutMetrics {
    /// Fixed height of every virtualized `uniform_list` commit row (CSS logical px).
    pub row_height: f32,
    /// Horizontal pitch between Metro lanes (CSS logical px).
    pub lane_width: f32,
    /// Left padding before lane `0` center (CSS logical px).
    pub padding_left: f32,
    /// Minimum graph column width regardless of lane count (CSS logical px).
    pub min_graph_width: f32,
    /// Metro rail stroke width (CSS logical px).
    pub stroke_width: f32,
    /// Standard commit node circle radius (CSS logical px).
    pub node_radius: f32,
    /// Larger `HEAD` commit node outer radius (CSS logical px).
    pub head_node_radius: f32,
    /// Virtual WIP node ring radius (CSS logical px).
    pub wip_node_radius: f32,
}

impl Default for GraphLayoutMetrics {
    fn default() -> Self {
        Self {
            row_height: 28.0,
            lane_width: 16.0,
            padding_left: 14.0,
            min_graph_width: 72.0,
            stroke_width: 2.0,
            node_radius: 4.5,
            head_node_radius: 6.0,
            wip_node_radius: 5.5,
        }
    }
}

/// 2D point in logical CSS pixels snapped to physical HiDPI boundaries.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphPoint {
    /// Horizontal coordinate in logical px.
    pub x: f32,
    /// Vertical coordinate in logical px.
    pub y: f32,
}

/// Straight rail segment drawn across a single commit row.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphLineSegment {
    /// Start point.
    pub from: GraphPoint,
    /// End point.
    pub to: GraphPoint,
    /// Packed `0xRRGGBBAA` stroke color.
    pub color_rgba: u32,
    /// Snapped stroke thickness in logical px.
    pub stroke_width: f32,
    /// True when rendered dashed (virtual WIP edge).
    pub dashed: bool,
}

/// Smooth cubic Bézier curve connecting two lanes during merge/fork transitions.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphBezierCurve {
    /// Entry point (`from_lane`).
    pub start: GraphPoint,
    /// First cubic control handle.
    pub ctrl1: GraphPoint,
    /// Second cubic control handle.
    pub ctrl2: GraphPoint,
    /// Exit point (`to_lane`).
    pub end: GraphPoint,
    /// Packed `0xRRGGBBAA` stroke color.
    pub color_rgba: u32,
    /// Snapped stroke thickness in logical px.
    pub stroke_width: f32,
    /// True when rendered dashed (virtual WIP edge).
    pub dashed: bool,
}

/// Commit or WIP circle geometry drawn on top of Metro rails.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct GraphNodeCircle {
    /// Circle center snapped to device pixels.
    pub center: GraphPoint,
    /// Outer radius in logical px.
    pub radius: f32,
    /// Packed `0xRRGGBBAA` fill color.
    pub fill_rgba: u32,
    /// Packed `0xRRGGBBAA` border ring color.
    pub border_rgba: u32,
    /// Structural commit glyph kind.
    pub kind: GraphNodeKind,
    /// True when representing the synthetic top WIP node.
    pub is_wip: bool,
    /// True if row represents `HEAD`.
    pub is_head: bool,
    /// True if currently selected in UI.
    pub is_selected: bool,
}

/// Complete vector geometry computed for one commit graph row slice `[0.0..row_height]`.
#[derive(Debug, Clone, PartialEq)]
pub struct GraphRowGeometry {
    /// Straight vertical rail segments.
    pub lines: Vec<GraphLineSegment>,
    /// Curved Metro lane transition segments.
    pub curves: Vec<GraphBezierCurve>,
    /// Central commit/WIP node circle.
    pub node: GraphNodeCircle,
    /// Total pixel width occupied by lanes + right gutter padding.
    pub lane_canvas_width: f32,
}

/// Snaps a logical pixel coordinate onto physical device pixel centers using [`DisplayScalePolicy`].
#[inline]
pub fn snap_logical_coord(logical_px: f32, scale_policy: DisplayScalePolicy) -> f32 {
    let dpr = scale_policy.scale_factor.max(0.5);
    (logical_px * dpr).round() / dpr
}

/// Computes Metro X center coordinate for lane index `lane`.
#[inline]
pub fn lane_center_x(
    lane: u16,
    metrics: GraphLayoutMetrics,
    scale_policy: DisplayScalePolicy,
) -> f32 {
    let raw = metrics.padding_left + (lane as f32) * metrics.lane_width;
    snap_logical_coord(raw, scale_policy)
}

/// Computes total pixel width required for `active_lane_count` Metro rails.
#[inline]
pub fn compute_lane_canvas_width(
    active_lane_count: u16,
    metrics: GraphLayoutMetrics,
    scale_policy: DisplayScalePolicy,
) -> f32 {
    let lanes = active_lane_count.max(1) as f32;
    let raw = metrics.padding_left * 2.0 + lanes * metrics.lane_width;
    snap_logical_coord(raw.max(metrics.min_graph_width), scale_policy)
}

/// Generates snapped vector geometry for the synthetic top [`VirtualWipRow`].
pub fn compute_wip_row_geometry(
    wip: &VirtualWipRow,
    metrics: GraphLayoutMetrics,
    scale_policy: DisplayScalePolicy,
    theme: &BranvioTheme,
    is_selected: bool,
) -> GraphRowGeometry {
    let h = snap_logical_coord(metrics.row_height, scale_policy);
    let cy = snap_logical_coord(h * 0.5, scale_policy);
    let bot_mid_y = snap_logical_coord((cy + h) * 0.5, scale_policy);
    let stroke = metrics.stroke_width.max(scale_policy.hairline_logical_px);

    let mut lines = Vec::with_capacity(wip.passing_lanes.len() + 1);
    let mut curves = Vec::with_capacity(1);

    for seg in &wip.passing_lanes {
        let x = lane_center_x(seg.lane_index, metrics, scale_policy);
        lines.push(GraphLineSegment {
            from: GraphPoint { x, y: 0.0 },
            to: GraphPoint { x, y: h },
            color_rgba: theme.metro_slot_rgba(seg.palette_slot),
            stroke_width: stroke,
            dashed: seg.is_dotted_wip,
        });
    }

    if let Some(edge) = wip.outgoing_head_edge {
        push_half_edge_geometry(
            edge,
            cy,
            h,
            bot_mid_y,
            stroke,
            true,
            metrics,
            scale_policy,
            theme,
            &mut lines,
            &mut curves,
        );
    } else if wip.head_oid.is_some() {
        let x = lane_center_x(wip.commit_lane, metrics, scale_policy);
        lines.push(GraphLineSegment {
            from: GraphPoint { x, y: cy },
            to: GraphPoint { x, y: h },
            color_rgba: theme.metro_slot_rgba(wip.palette_slot),
            stroke_width: stroke,
            dashed: true,
        });
    }

    let node_x = lane_center_x(wip.commit_lane, metrics, scale_policy);
    let node = GraphNodeCircle {
        center: GraphPoint { x: node_x, y: cy },
        radius: metrics.wip_node_radius,
        fill_rgba: theme.wip_node,
        border_rgba: if is_selected {
            theme.text_primary
        } else {
            theme.wip_node
        },
        kind: GraphNodeKind::Regular,
        is_wip: true,
        is_head: false,
        is_selected,
    };

    let lane_canvas_width =
        compute_lane_canvas_width(wip.max_active_lane.saturating_add(1), metrics, scale_policy);

    GraphRowGeometry {
        lines,
        curves,
        node,
        lane_canvas_width,
    }
}

/// Generates snapped vector geometry for a historical [`GraphRow`] inside `[0.0..metrics.row_height]`.
pub fn compute_commit_row_geometry(
    row: &GraphRow,
    metrics: GraphLayoutMetrics,
    scale_policy: DisplayScalePolicy,
    theme: &BranvioTheme,
    is_head: bool,
    is_selected: bool,
) -> GraphRowGeometry {
    let h = snap_logical_coord(metrics.row_height, scale_policy);
    let cy = snap_logical_coord(h * 0.5, scale_policy);
    let stroke = metrics.stroke_width.max(scale_policy.hairline_logical_px);

    let mut lines = Vec::with_capacity(
        row.passing_lanes.len() + row.incoming_edges.len() + row.outgoing_edges.len(),
    );
    let mut curves = Vec::with_capacity(row.incoming_edges.len() + row.outgoing_edges.len());

    for seg in &row.passing_lanes {
        let x = lane_center_x(seg.lane_index, metrics, scale_policy);
        lines.push(GraphLineSegment {
            from: GraphPoint { x, y: 0.0 },
            to: GraphPoint { x, y: h },
            color_rgba: theme.metro_slot_rgba(seg.palette_slot),
            stroke_width: stroke,
            dashed: seg.is_dotted_wip,
        });
    }

    let top_mid_y = snap_logical_coord(cy * 0.5, scale_policy);
    for &edge in &row.incoming_edges {
        push_half_edge_geometry(
            edge,
            0.0,
            cy,
            top_mid_y,
            stroke,
            false,
            metrics,
            scale_policy,
            theme,
            &mut lines,
            &mut curves,
        );
    }

    let bot_mid_y = snap_logical_coord((cy + h) * 0.5, scale_policy);
    for &edge in &row.outgoing_edges {
        push_half_edge_geometry(
            edge,
            cy,
            h,
            bot_mid_y,
            stroke,
            false,
            metrics,
            scale_policy,
            theme,
            &mut lines,
            &mut curves,
        );
    }

    let node_x = lane_center_x(row.commit_lane, metrics, scale_policy);
    let fill_rgba = match row.node_kind {
        GraphNodeKind::StashNode { .. } => theme.accent_secondary,
        _ => theme.metro_slot_rgba(row.node_palette_slot),
    };
    let radius = if is_head {
        metrics.head_node_radius
    } else {
        metrics.node_radius
    };
    let border_rgba = if is_head {
        theme.head_accent
    } else if is_selected {
        theme.text_primary
    } else {
        theme.bg_base
    };

    let node = GraphNodeCircle {
        center: GraphPoint { x: node_x, y: cy },
        radius,
        fill_rgba,
        border_rgba,
        kind: row.node_kind,
        is_wip: false,
        is_head,
        is_selected,
    };

    let lane_canvas_width =
        compute_lane_canvas_width(row.max_active_lane.saturating_add(1), metrics, scale_policy);

    GraphRowGeometry {
        lines,
        curves,
        node,
        lane_canvas_width,
    }
}

#[allow(clippy::too_many_arguments)]
fn push_half_edge_geometry(
    edge: GraphEdge,
    y_start: f32,
    y_end: f32,
    mid_y: f32,
    stroke_width: f32,
    dashed: bool,
    metrics: GraphLayoutMetrics,
    scale_policy: DisplayScalePolicy,
    theme: &BranvioTheme,
    lines: &mut Vec<GraphLineSegment>,
    curves: &mut Vec<GraphBezierCurve>,
) {
    let x1 = lane_center_x(edge.from_lane, metrics, scale_policy);
    let x2 = lane_center_x(edge.to_lane, metrics, scale_policy);
    let color_rgba = theme.metro_slot_rgba(edge.palette_slot);

    if edge.curve == EdgeCurveKind::Straight || (x1 - x2).abs() <= f32::EPSILON {
        lines.push(GraphLineSegment {
            from: GraphPoint { x: x1, y: y_start },
            to: GraphPoint { x: x2, y: y_end },
            color_rgba,
            stroke_width,
            dashed,
        });
    } else {
        curves.push(GraphBezierCurve {
            start: GraphPoint { x: x1, y: y_start },
            ctrl1: GraphPoint { x: x1, y: mid_y },
            ctrl2: GraphPoint { x: x2, y: mid_y },
            end: GraphPoint { x: x2, y: y_end },
            color_rgba,
            stroke_width,
            dashed,
        });
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use branvio_core::domain::{GraphEdge, LaneSegment};
    use branvio_core::ids::GitOid;

    #[test]
    fn hidpi_snaps_coordinates_deterministically() {
        let p_1x = DisplayScalePolicy::from_scale_factor(1.0);
        let p_2x = DisplayScalePolicy::from_scale_factor(2.0);
        let p_15x = DisplayScalePolicy::from_scale_factor(1.5);

        assert!((snap_logical_coord(14.3, p_1x) - 14.0).abs() < 1e-5);
        assert!((snap_logical_coord(14.4, p_2x) - 14.5).abs() < 1e-5);
        assert!((snap_logical_coord(14.4, p_15x) * 1.5 - 22.0).abs() < 1e-4);
    }

    #[test]
    fn generates_smooth_bezier_and_wip_dashed_geometry() {
        let theme = BranvioTheme::dark();
        let metrics = GraphLayoutMetrics::default();
        let scale = DisplayScalePolicy::from_scale_factor(2.0);
        let oid = GitOid::from_hex("1111111111111111111111111111111111111111").unwrap();

        let wip = VirtualWipRow {
            head_oid: Some(oid),
            head_commit_row: Some(0),
            commit_lane: 0,
            max_active_lane: 1,
            palette_slot: 0,
            staged_count: 1,
            unstaged_count: 2,
            conflicted_count: 0,
            untracked_count: 1,
            passing_lanes: Default::default(),
            outgoing_head_edge: None,
        };
        let wip_geom = compute_wip_row_geometry(&wip, metrics, scale, &theme, true);
        assert_eq!(wip_geom.lines.len(), 1);
        assert!(wip_geom.lines[0].dashed);
        assert!(wip_geom.node.is_wip);
        assert!(wip_geom.node.is_selected);

        let row = GraphRow {
            oid,
            commit_lane: 0,
            max_active_lane: 1,
            node_kind: GraphNodeKind::Merge,
            node_palette_slot: 0,
            passing_lanes: [LaneSegment {
                lane_index: 1,
                palette_slot: 4,
                is_dotted_wip: false,
            }]
            .into_iter()
            .collect(),
            incoming_edges: Default::default(),
            outgoing_edges: [GraphEdge {
                target_row: 1,
                from_lane: 0,
                to_lane: 1,
                palette_slot: 4,
                curve: EdgeCurveKind::ForkRight,
                is_merge_edge: true,
            }]
            .into_iter()
            .collect(),
        };

        let row_geom = compute_commit_row_geometry(&row, metrics, scale, &theme, true, false);
        assert_eq!(row_geom.lines.len(), 1);
        assert_eq!(row_geom.curves.len(), 1);
        assert_eq!(row_geom.curves[0].ctrl1.x, row_geom.curves[0].start.x);
        assert_eq!(row_geom.curves[0].ctrl2.x, row_geom.curves[0].end.x);
        assert!(row_geom.node.is_head);
    }
}
