pub trait Area {
    fn area(&self) -> f64;
}

pub struct Circle {
    pub r: f64,
}

impl Area for Circle {
    fn area(&self) -> f64 {
        std::f64::consts::PI * self.r * self.r
    }
}
