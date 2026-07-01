//! Top-down orthographic camera measured in tile units.

use glam::{Mat4, Vec2};

pub struct Camera {
    /// World-space point at the centre of the screen (in tiles).
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
        Mat4::orthographic_rh(left, right, bottom, top, -1.0, 1.0)
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
        self.view_height = (self.view_height * factor).clamp(8.0, 80.0);
    }
}
