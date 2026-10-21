//! Top-down orthographic camera measured in tile units.

// wgpu clip space has Z in [0, 1], which matches glam's DirectX convention.
use glam::camera::rh::proj::directx;
use glam::{Mat4, Vec2};

/// Closest zoom, in tiles of view height.
pub const MIN_VIEW_HEIGHT: f32 = 8.0;
/// Farthest zoom. Past `DETAIL_VIEW_HEIGHT` the map switches to the flat
/// overview, so this can reach well beyond what full sprites could afford.
pub const MAX_VIEW_HEIGHT: f32 = 360.0;
/// The farthest zoom drawn with full textures and sprites.
pub const DETAIL_VIEW_HEIGHT: f32 = 80.0;

pub struct Camera {
    /// World-space point at the center of the screen (in tiles).
    pub center: Vec2,
    /// How many tiles tall the viewport is; width derives from aspect.
    pub view_height: f32,
    pub aspect: f32,
}

impl Camera {
    pub fn new(center: Vec2) -> Self {
        Self {
            center,
            view_height: 28.0,
            aspect: 1.0,
        }
    }

    fn half_extents(&self) -> Vec2 {
        let hh = self.view_height * 0.5;
        Vec2::new(hh * self.aspect, hh)
    }

    /// Orthographic view-projection with +Y pointing *down* the screen so tile
    /// (0,0) is top-left, matching the world grid.
    pub fn view_proj(&self) -> Mat4 {
        let he = self.half_extents();
        let left = self.center.x - he.x;
        let right = self.center.x + he.x;
        let top = self.center.y - he.y;
        let bottom = self.center.y + he.y;
        directx::orthographic(left, right, bottom, top, -1.0, 1.0)
    }

    /// Convert a screen pixel coordinate to world-space tile coordinates.
    pub fn screen_to_world(&self, px: f32, py: f32, screen_w: f32, screen_h: f32) -> Vec2 {
        let he = self.half_extents();
        let nx = px / screen_w; // 0..1 left->right
        let ny = py / screen_h; // 0..1 top->bottom
        Vec2::new(
            self.center.x - he.x + nx * he.x * 2.0,
            self.center.y - he.y + ny * he.y * 2.0,
        )
    }

    pub fn zoom(&mut self, factor: f32) {
        self.view_height = (self.view_height * factor).clamp(MIN_VIEW_HEIGHT, MAX_VIEW_HEIGHT);
    }

    /// Is the camera zoomed out past full detail, into the flat overview?
    pub fn overview(&self) -> bool {
        self.view_height > DETAIL_VIEW_HEIGHT
    }

    /// Half the view's size in tiles (width, height).
    pub fn half_size(&self) -> Vec2 {
        self.half_extents()
    }
}
