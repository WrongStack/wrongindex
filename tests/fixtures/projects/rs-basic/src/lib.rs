pub mod shapes;

use shapes::{Area, Circle as C};

/// Total area of a slice of shapes.
pub fn total<T: Area>(items: &[T]) -> f64 {
    items.iter().map(|i| i.area()).sum()
}

macro_rules! square {
    ($x:expr) => {
        $x * $x
    };
}

pub fn demo() -> f64 {
    let c = C { r: 2.0 };
    c.area() + square!(2.0)
}
