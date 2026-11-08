use relm4::adw::prelude::*;
use relm4::{adw, gtk};
fn main() {
    gtk::init().unwrap();
    adw::init().unwrap();
    let r = {
        let row = adw::ActionRow::builder().use_markup(false).subtitle_lines(1).build();
        row.set_title(&("a & b"));
        row.set_subtitle(&("c & d"));
        row
    };
    let e = adw::ExpanderRow::builder().use_markup(false).build();
    e.set_title("x & y");
    e.set_subtitle("z & w");
    let w = gtk::Window::new();
    let l = gtk::ListBox::new();
    l.append(&r);
    l.append(&e);
    w.set_child(Some(&l));
    w.present();
    let ctx = gtk::glib::MainContext::default();
    for _ in 0..50 {
        ctx.iteration(false);
    }
    eprintln!("probe done");
}
