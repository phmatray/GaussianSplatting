use gs_core::rig::ScanPath;
fn main() {
    let path = ScanPath::house_default();
    let pts = path.polyline(6000);
    let mut last_print = -100i32;
    for (i, w) in pts.windows(3).enumerate() {
        let (a, b) = ((w[1] - w[0]).normalize(), (w[2] - w[1]).normalize());
        let ang = a.angle_between(b);
        let step = (w[1] - w[0]).length();
        if ang > 0.02 && i as i32 - last_print > 20 {
            println!("i={i} pos=({:.2},{:.2}) angle={ang:.3} radius={:.3}", w[1].x, w[1].z, step / ang);
            last_print = i as i32;
        }
    }
}
