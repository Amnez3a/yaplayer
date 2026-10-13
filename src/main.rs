use gtk4::prelude::*;
use gtk4::{
    Application, ApplicationWindow, Box as GtkBox, Button, CenterBox, CssProvider, Entry, Label,
    Orientation, Overlay, Scale, ScrolledWindow, Align, Adjustment, Picture, Image, PolicyType,
    Switch, MenuButton, Popover, DropDown, StringList, StringObject, LinkButton
};
use gtk4::gdk::Display;
use gtk4::glib;
use gtk4::gio;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::process::{Child, Command};
use std::io::{Write, BufRead};
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::time::{SystemTime, UNIX_EPOCH};
use std::path::{Path, PathBuf};


const APP_ID: &str = "com.yatavo.player";

fn get_app_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = PathBuf::from(home).join("Music").join("yaplayer");
    std::fs::create_dir_all(&path).ok();
    path
}

fn get_fav_file() -> PathBuf {
    get_app_dir().join("favorites.json")
}

fn get_config_file() -> PathBuf {
    get_app_dir().join("settings.json")
}

fn default_eq_bands() -> [f64; 10] {
    [0.0; 10]
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AppConfig {
    #[serde(default)]
    auto_download: bool,
    #[serde(default)]
    custom_wallpapers: bool,
    #[serde(default)]
    wallpapers_path: String,
    #[serde(default = "default_eq_bands")]
    eq_bands: [f64; 10], 
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            auto_download: false,
            custom_wallpapers: false,
            wallpapers_path: String::new(),
            eq_bands: default_eq_bands(),
        }
    }
}

fn load_config() -> AppConfig {
    if let Ok(data) = std::fs::read_to_string(get_config_file()) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        AppConfig::default()
    }
}

fn save_config(config: &AppConfig) {
    if let Ok(data) = serde_json::to_string_pretty(config) {
        let _ = std::fs::write(get_config_file(), data);
    }
}

fn build_eq_af(bands: &[f64; 10]) -> String {
    let freqs = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
    let mut parts = Vec::new();
    for (i, &f) in freqs.iter().enumerate() {
        parts.push(format!("equalizer=f={}:width_type=o:width=1:g={}", f, bands[i]));
    }
    parts.join(",")
}

fn update_wallpaper(enable: bool, raw_path: &str, provider: &CssProvider) {
    let clean_path = raw_path.trim();
    let expanded_path = if clean_path.starts_with("~/") {
        let home = std::env::var("HOME").unwrap_or_default();
        clean_path.replacen("~", &home, 1)
    } else {
        clean_path.to_string()
    };

    let p = Path::new(&expanded_path);
    if enable && !expanded_path.is_empty() && p.exists() {
        let css = format!(
            "window, .app-background {{
                background-image: url('file://{}');
                background-size: cover;
                background-position: center;
                background-repeat: no-repeat;
            }}",
            expanded_path
        );
        provider.load_from_data(&css);
    } else {
        provider.load_from_data(
            "window, .app-background {
                background-image: none;
                background-color: @theme_bg_color;
            }"
        );
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Thumbnail {
    url: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct Track {
    title: Option<String>,
    url: Option<String>,
    webpage_url: Option<String>,
    duration: Option<f64>,
    thumbnail: Option<String>,
    thumbnails: Option<Vec<Thumbnail>>,
    #[serde(default)]
    platform: Option<String>, 
}

impl Track {
    fn get_url(&self) -> String {
        self.webpage_url.clone().unwrap_or_else(|| self.url.clone().unwrap_or_default())
    }
    
    fn get_thumbnail(&self) -> String {
        if let Some(t) = &self.thumbnail { return t.clone(); }
        if let Some(ts) = &self.thumbnails {
            if let Some(t) = ts.last() { return t.url.clone().unwrap_or_default(); }
        }
        "".to_string()
    }
}

#[derive(Serialize, Deserialize, Default)]
struct Favorites {
    local: Vec<Track>,
    streaming: Vec<Track>,
}

fn load_favorites() -> Favorites {
    if let Ok(data) = std::fs::read_to_string(get_fav_file()) {
        serde_json::from_str(&data).unwrap_or_default()
    } else {
        Favorites::default()
    }
}

fn save_favorites(favs: &Favorites) {
    if let Ok(data) = serde_json::to_string_pretty(favs) {
        let _ = std::fs::write(get_fav_file(), data);
    }
}

struct PlayerState {
    process: Option<Child>,
    tracks: Vec<Track>,
    current_idx: usize,
    current_track: Option<Track>,
    is_playing: bool,
    last_played_url: String, 
}

enum PlaybackUpdate {
    TimePos(f64),
    Duration(f64),
    Eof, 
}

fn format_time(secs: f64) -> String {
    if secs.is_nan() || secs < 0.0 { return "00:00".to_string(); }
    let s = secs as u64;
    let m = s / 60;
    let s = s % 60;
    format!("{:02}:{:02}", m, s)
}

fn play_track_obj(
    state: &Rc<RefCell<PlayerState>>,
    track: Track,
    playlist_idx: Option<usize>,
    play_btn: &Button,
    title_label: &Label,
    vol_slider: &Scale,
    progress_adj: &Adjustment,
    lbl_left: &Label,
    lbl_right: &Label,
    cover_picture: &Picture,
    history_list: &GtkBox,
    pb_tx: &async_channel::Sender<PlaybackUpdate>,
    star_btn: &Button,
) {
    let mut st = state.borrow_mut();
    
    if let Some(idx) = playlist_idx {
        st.current_idx = idx;
    }
    
    st.current_track = Some(track.clone());
    
    let url = track.get_url();
    let title = track.title.clone().unwrap_or_else(|| "Неизвестно".into());
    let thumb_url = track.get_thumbnail();

    let is_duplicate = st.last_played_url == url;
    st.last_played_url = url.clone();

    st.is_playing = true;

    let favs = load_favorites();
    let is_fav = if url.starts_with("http") {
        favs.streaming.iter().any(|t| t.get_url() == url)
    } else {
        favs.local.iter().any(|t| t.get_url() == url)
    };
    star_btn.set_icon_name(if is_fav { "starred-symbolic" } else { "non-starred-symbolic" });

    progress_adj.set_value(0.0);
    lbl_left.set_label("00:00");
    lbl_right.set_label(&format_time(track.duration.unwrap_or(0.0)));
    title_label.set_label(&title);
    
    play_btn.set_icon_name("media-playback-pause-symbolic");

    if let Some(mut p) = st.process.take() {
        let _ = p.kill();
        let _ = p.wait();
    }

    let current_vol = vol_slider.value() as i32;
    let conf = load_config();
    
    let af_arg = format!("--af={}", build_eq_af(&conf.eq_bands));

    if let Ok(child) = Command::new("mpv")
        .args([
            "--no-video", 
            "--ytdl-format=bestaudio", 
            "--really-quiet", 
            "--input-ipc-server=/tmp/yaplayer_ipc.sock", 
            &format!("--volume={}", current_vol), 
            &af_arg,
            &url
        ])
        .spawn()
    {
        st.process = Some(child);
    }

    if url.starts_with("http") && conf.auto_download {
        let dir = get_app_dir();
        let url_clone = url.clone();
        std::thread::spawn(move || {
            let _ = Command::new("yt-dlp")
                .args(["-x", "--audio-format", "mp3", "-o", &format!("{}/%(title)s.%(ext)s", dir.display()), &url_clone])
                .output();
        });
    }

    let tx = pb_tx.clone();
    std::thread::spawn(move || {
        let mut attempts = 0;
        let stream = loop {
            if let Ok(s) = UnixStream::connect("/tmp/yaplayer_ipc.sock") {
                break Some(s);
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            attempts += 1;
            if attempts > 40 { break None; } 
        };
        
        if let Some(mut stream) = stream {
            let observe_cmds = "{\"command\": [\"observe_property\", 1, \"time-pos\"]}\n{\"command\": [\"observe_property\", 2, \"duration\"]}\n";
            let _ = stream.write_all(observe_cmds.as_bytes());
            
            let reader = std::io::BufReader::new(stream);
            for line in reader.lines() {
                if let Ok(l) = line {
                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(&l) {
                        if val["event"] == "property-change" {
                            if val["name"] == "time-pos" {
                                if let Some(time) = val["data"].as_f64() {
                                    let _ = tx.send_blocking(PlaybackUpdate::TimePos(time));
                                }
                            } else if val["name"] == "duration" {
                                if let Some(dur) = val["data"].as_f64() {
                                    let _ = tx.send_blocking(PlaybackUpdate::Duration(dur));
                                }
                            }
                        } else if val["event"] == "end-file" && val["reason"] == "eof" {
                            let _ = tx.send_blocking(PlaybackUpdate::Eof);
                        }
                    }
                }
            }
        }
    });

    if !is_duplicate {
        let hist_btn = Button::builder().css_classes(["history-item-btn"]).build();
        let hist_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
        
        let title_lbl = Label::builder().label(&title).halign(Align::Start).valign(Align::Center).hexpand(true).ellipsize(gtk4::pango::EllipsizeMode::End).css_classes(["history-title"]).build();
        let chevron = Image::builder().icon_name("go-next-symbolic").css_classes(["muted-icon"]).valign(Align::Center).build();
        
        let icon_name = if url.starts_with("http") { "network-wireless-symbolic" } else { "audio-x-generic-symbolic" };
        let source_icon = Image::builder().icon_name(icon_name).css_classes(["muted-icon"]).width_request(24).height_request(24).build();
        
        let platform_str = track.platform.clone().unwrap_or_else(|| if url.starts_with("http") { "YouTube".to_string() } else { "Local".to_string() });
        let platform_lbl = Label::builder().label(&platform_str).css_classes(["history-platform"]).valign(Align::Center).build();

        hist_box.append(&source_icon);
        hist_box.append(&title_lbl);
        hist_box.append(&platform_lbl);
        hist_box.append(&chevron);
        hist_btn.set_child(Some(&hist_box));
        
        let track_clone = track.clone();
        let st_click = state.clone();
        let play_btn_click = play_btn.clone();
        let title_lbl_click = title_label.clone();
        let vol_slider_click = vol_slider.clone();
        let progress_adj_click = progress_adj.clone();
        let lbl_l_click = lbl_left.clone();
        let lbl_r_click = lbl_right.clone();
        let cover_pic_click = cover_picture.clone();
        let hist_list_click = history_list.clone();
        let pb_tx_click = pb_tx.clone();
        let star_btn_click = star_btn.clone();
        
        hist_btn.connect_clicked(move |_| {
            play_track_obj(&st_click, track_clone.clone(), None, &play_btn_click, &title_lbl_click, &vol_slider_click, &progress_adj_click, &lbl_l_click, &lbl_r_click, &cover_pic_click, &hist_list_click, &pb_tx_click, &star_btn_click);
        });
        
        history_list.prepend(&hist_btn);

        let mut count = 0;
        let mut child = history_list.first_child();
        while let Some(c) = child {
            count += 1;
            let next = c.next_sibling();
            if count > 7 { history_list.remove(&c); }
            child = next;
        }

        if !thumb_url.is_empty() {
            let (tx, rx) = async_channel::bounded::<String>(1);
            let cover_clone = cover_picture.clone();
            
            glib::spawn_future_local(async move {
                if let Ok(path) = rx.recv().await {
                    let file = gio::File::for_path(&path);
                    cover_clone.set_file(Some(&file));
                }
            });
            
            std::thread::spawn(move || {
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                let path = format!("/tmp/yaplayer_thumb_{}.jpg", ts);
                let _ = Command::new("curl").args(["-s", "--max-time", "5", "-o", &path, &thumb_url]).output();
                let _ = tx.send_blocking(path);
            });
        } else {
            cover_picture.set_file(None::<&gio::File>);
        }
    } else {
        if !thumb_url.is_empty() {
            let (tx, rx) = async_channel::bounded::<String>(1);
            let cover_clone = cover_picture.clone();
            glib::spawn_future_local(async move {
                if let Ok(path) = rx.recv().await {
                    let file = gio::File::for_path(&path);
                    cover_clone.set_file(Some(&file));
                }
            });
            std::thread::spawn(move || {
                let ts = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis();
                let path = format!("/tmp/yaplayer_thumb_{}.jpg", ts);
                let _ = Command::new("curl").args(["-s", "--max-time", "5", "-o", &path, &thumb_url]).output();
                let _ = tx.send_blocking(path);
            });
        } else {
            cover_picture.set_file(None::<&gio::File>);
        }
    }
}

fn render_favorites_list(
    list_box: &GtkBox,
    tracks: Vec<Track>,
    is_local: bool,
    state: &Rc<RefCell<PlayerState>>,
    play_btn: &Button,
    title_label: &Label,
    vol_slider: &Scale,
    progress_adj: &Adjustment,
    lbl_left: &Label,
    lbl_right: &Label,
    cover_picture: &Picture,
    history_list: &GtkBox,
    pb_tx: &async_channel::Sender<PlaybackUpdate>,
    star_btn: &Button,
) {
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }
    for track in tracks {
        let item_btn = Button::builder().css_classes(["history-item-btn"]).build();
        let item_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
        
        let icon_name = if is_local { "audio-x-generic-symbolic" } else { "network-wireless-symbolic" };
        let icon = Image::builder().icon_name(icon_name).css_classes(["muted-icon"]).valign(Align::Center).build();
        
        let title_lbl = Label::builder()
            .label(&track.title.clone().unwrap_or_default())
            .halign(Align::Start).valign(Align::Center).hexpand(true)
            .ellipsize(gtk4::pango::EllipsizeMode::End).css_classes(["history-title"])
            .build();
            
        let chevron = Image::builder().icon_name("go-next-symbolic").css_classes(["muted-icon"]).valign(Align::Center).build();
        
        let platform_str = track.platform.clone().unwrap_or_else(|| if is_local { "Local".to_string() } else { "YouTube".to_string() });
        let platform_lbl = Label::builder().label(&platform_str).css_classes(["history-platform"]).valign(Align::Center).build();

        item_box.append(&icon);
        item_box.append(&title_lbl);
        item_box.append(&platform_lbl);
        item_box.append(&chevron);
        item_btn.set_child(Some(&item_box));
        
        let track_clone = track.clone();
        let st_click = state.clone();
        let play_btn_click = play_btn.clone();
        let title_lbl_click = title_label.clone();
        let vol_slider_click = vol_slider.clone();
        let progress_adj_click = progress_adj.clone();
        let lbl_l_click = lbl_left.clone();
        let lbl_r_click = lbl_right.clone();
        let cover_pic_click = cover_picture.clone();
        let hist_list_click = history_list.clone();
        let pb_tx_click = pb_tx.clone();
        let star_btn_click = star_btn.clone();
        
        item_btn.connect_clicked(move |_| {
            play_track_obj(&st_click, track_clone.clone(), None, &play_btn_click, &title_lbl_click, &vol_slider_click, &progress_adj_click, &lbl_l_click, &lbl_r_click, &cover_pic_click, &hist_list_click, &pb_tx_click, &star_btn_click);
        });
        
        list_box.append(&item_btn);
    }
}

fn main() {
    let app = Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(build_ui);
    app.run();
}

fn build_ui(app: &Application) {
    let overlay = Overlay::new();
    
    let (pb_tx, pb_rx) = async_channel::unbounded::<PlaybackUpdate>();
    
    let player_state = Rc::new(RefCell::new(PlayerState {
        process: None,
        tracks: vec![],
        current_idx: 0,
        current_track: None,
        is_playing: false,
        last_played_url: String::new(),
    }));


    let title_label = Label::builder()
        .label("Song Name")
        .css_classes(["song-title"])
        .halign(Align::Center)
        .valign(Align::Center)
        .width_request(500)
        .height_request(75) 
        .ellipsize(gtk4::pango::EllipsizeMode::End)
        .wrap(true)
        .wrap_mode(gtk4::pango::WrapMode::WordChar)
        .justify(gtk4::Justification::Center)
        .build();

    let artist_label = Label::builder().label("Artist").css_classes(["song-artist"]).halign(Align::Center).build();

    let play_btn = Button::builder().icon_name("media-playback-start-symbolic").css_classes(["player-btn-main"]).build();
    let prev_btn = Button::builder().icon_name("media-skip-backward-symbolic").css_classes(["player-btn"]).build();
    let next_btn = Button::builder().icon_name("media-skip-forward-symbolic").css_classes(["player-btn"]).build();
    let vol_slider = Scale::builder().orientation(Orientation::Horizontal).adjustment(&Adjustment::new(50.0, 0.0, 100.0, 1.0, 10.0, 0.0)).width_request(120).build();
    vol_slider.set_draw_value(false);

    let star_btn = Button::builder().icon_name("non-starred-symbolic").css_classes(["flat-icon-btn"]).build();
    let dl_btn = Button::builder().icon_name("go-down-symbolic").css_classes(["flat-icon-btn"]).build();

    let time_lbl_left = Label::builder().label("00:00").css_classes(["time-label"]).valign(Align::Center).build();
    let time_lbl_right = Label::builder().label("00:00").css_classes(["time-label"]).valign(Align::Center).build();
    
    let progress_adj = Adjustment::new(0.0, 0.0, 100.0, 1.0, 10.0, 0.0);
    let progress = Scale::builder().orientation(Orientation::Horizontal).adjustment(&progress_adj).width_request(350).build();
    progress.set_draw_value(false);

    progress.connect_change_value(move |_, _, val| {
        if let Ok(mut stream) = UnixStream::connect("/tmp/yaplayer_ipc.sock") {
            let cmd = format!("{{\"command\": [\"set_property\", \"time-pos\", {}]}}\n", val);
            let _ = stream.write_all(cmd.as_bytes());
        }
        glib::Propagation::Proceed
    });

    let cover_picture = Picture::builder().css_classes(["cover-art-pic"]).width_request(400).height_request(400).halign(Align::Center).valign(Align::Center).build();
    cover_picture.set_keep_aspect_ratio(true);
    cover_picture.set_can_shrink(true);

    let st_play = player_state.clone();
    play_btn.connect_clicked(move |btn| {
        let mut st = st_play.borrow_mut();
        st.is_playing = !st.is_playing;
        let is_playing = st.is_playing; 
        
        if st.process.is_some() {
            if let Ok(mut stream) = UnixStream::connect("/tmp/yaplayer_ipc.sock") {
                let pause_state = if is_playing { "false" } else { "true" };
                let cmd = format!("{{\"command\": [\"set_property\", \"pause\", {}]}}\n", pause_state);
                let _ = stream.write_all(cmd.as_bytes());
            }
            btn.set_icon_name(if is_playing { "media-playback-pause-symbolic" } else { "media-playback-start-symbolic" });
        }
    });

    let st_vol = player_state.clone();
    vol_slider.connect_value_changed(move |scale| {
        let val = scale.value() as i32;
        if st_vol.borrow().process.is_some() {
            if let Ok(mut stream) = UnixStream::connect("/tmp/yaplayer_ipc.sock") {
                let cmd = format!("{{\"command\": [\"set_property\", \"volume\", {}]}}\n", val);
                let _ = stream.write_all(cmd.as_bytes());
            }
        }
    });

    let main_vbox = GtkBox::builder().orientation(Orientation::Vertical).css_classes(["app-background"]).build();
    overlay.set_child(Some(&main_vbox));

    let columns_hbox = GtkBox::builder().orientation(Orientation::Horizontal).spacing(20).margin_top(20).margin_start(20).margin_end(20).margin_bottom(20).vexpand(true).build();
    main_vbox.append(&columns_hbox);

    let left_col = GtkBox::builder().orientation(Orientation::Vertical).spacing(15).width_request(280).hexpand(false).build();
    let top_left_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(10).build();
    
    let settings_btn = MenuButton::builder().icon_name("emblem-system-symbolic").css_classes(["menu-btn"]).build();
    let eq_btn = MenuButton::builder().label("EQ").css_classes(["menu-btn"]).build();
    let play_folder_btn = Button::builder().label("▶ Local folder").css_classes(["icon-btn"]).build();
    
    top_left_box.append(&settings_btn);
    top_left_box.append(&eq_btn);
    top_left_box.append(&play_folder_btn);
    
    let (panel_fav, fav_list) = create_list_panel("starred-symbolic", "Local favorite");
    let (panel_stream, stream_list) = create_list_panel("network-wireless-symbolic", "Streaming services");
    
    left_col.append(&top_left_box);
    left_col.append(&panel_fav);
    left_col.append(&panel_stream);
    columns_hbox.append(&left_col);

    let center_col = GtkBox::builder().orientation(Orientation::Vertical).hexpand(true).vexpand(true).build();
    let search_box = GtkBox::builder().orientation(Orientation::Horizontal).css_classes(["search-bar"]).spacing(10).margin_start(40).margin_end(40).build();
    
    let source_options = StringList::new(&["YouTube", "Spotify"]);
    let source_dropdown = DropDown::new(Some(source_options), None::<gtk4::Expression>);
    source_dropdown.add_css_class("search-dropdown");
    
    search_box.append(&source_dropdown);
    
    let search_entry = Entry::builder().placeholder_text("Нажми Enter для поиска...").hexpand(true).css_classes(["search-entry"]).build();
    search_box.append(&search_entry);
    search_box.append(&Image::builder().icon_name("system-search-symbolic").css_classes(["muted-icon"]).build());
    center_col.append(&search_box);

    let results_container = GtkBox::builder().orientation(Orientation::Vertical).css_classes(["results-container"]).build();
    
    let search_popover = Popover::builder()
        .child(&results_container)
        .position(gtk4::PositionType::Bottom)
        .build();
    search_popover.set_parent(&search_box);

    let player_view = GtkBox::builder().orientation(Orientation::Vertical).halign(Align::Center).valign(Align::Center).vexpand(true).spacing(15).build();
    player_view.append(&cover_picture);
    player_view.append(&title_label);
    player_view.append(&artist_label);
    center_col.append(&player_view);
    columns_hbox.append(&center_col);

    let right_col = GtkBox::builder().orientation(Orientation::Vertical).width_request(280).hexpand(false).build();
    let (hist_panel, history_list) = create_list_panel("view-restore-symbolic", "History");
    right_col.append(&hist_panel);
    columns_hbox.append(&right_col);

    let st_eof = player_state.clone();
    let play_btn_eof = play_btn.clone();
    let title_eof = title_label.clone();
    let vol_eof = vol_slider.clone();
    let prog_eof = progress_adj.clone();
    let lbl_l_eof = time_lbl_left.clone();
    let lbl_r_eof = time_lbl_right.clone();
    let cover_eof = cover_picture.clone();
    let hist_list_eof = history_list.clone();
    let pb_tx_eof = pb_tx.clone();
    let star_btn_eof = star_btn.clone();
    
    glib::spawn_future_local(async move {
        while let Ok(msg) = pb_rx.recv().await {
            match msg {
                PlaybackUpdate::TimePos(t) => {
                    prog_eof.set_value(t);
                    lbl_l_eof.set_label(&format_time(t));
                }
                PlaybackUpdate::Duration(d) => {
                    prog_eof.set_upper(d);
                    lbl_r_eof.set_label(&format_time(d));
                }
                PlaybackUpdate::Eof => {
                    let (track, idx) = {
                        let st = st_eof.borrow();
                        if st.tracks.is_empty() { continue; }
                        let new_idx = (st.current_idx + 1) % st.tracks.len();
                        (st.tracks[new_idx].clone(), new_idx)
                    };
                    play_track_obj(&st_eof, track, Some(idx), &play_btn_eof, &title_eof, &vol_eof, &prog_eof, &lbl_l_eof, &lbl_r_eof, &cover_eof, &hist_list_eof, &pb_tx_eof, &star_btn_eof);
                }
            }
        }
    });

    let st_folder = player_state.clone();
    let play_btn_folder = play_btn.clone();
    let title_folder = title_label.clone();
    let vol_folder = vol_slider.clone();
    let prog_folder = progress_adj.clone();
    let lbl_l_folder = time_lbl_left.clone();
    let lbl_r_folder = time_lbl_right.clone();
    let cover_folder = cover_picture.clone();
    let hist_list_folder = history_list.clone();
    let pb_tx_folder = pb_tx.clone();
    let star_btn_folder = star_btn.clone();
    
    play_folder_btn.connect_clicked(move |_| {
        let mut tracks = vec![];
        let dir = get_app_dir();
        
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                if let Ok(name) = entry.file_name().into_string() {
                    if name.ends_with(".mp3") {
                        tracks.push(Track {
                            title: Some(name.replace(".mp3", "")),
                            url: Some(entry.path().to_string_lossy().into_owned()),
                            webpage_url: None, duration: Some(0.0), thumbnail: None, thumbnails: None,
                            platform: Some("Local".to_string()),
                        });
                    }
                }
            }
        }
        
        if !tracks.is_empty() {
            tracks.sort_by(|a, b| a.title.cmp(&b.title));
            st_folder.borrow_mut().tracks = tracks.clone();
            play_track_obj(&st_folder, tracks[0].clone(), Some(0), &play_btn_folder, &title_folder, &vol_folder, &prog_folder, &lbl_l_folder, &lbl_r_folder, &cover_folder, &hist_list_folder, &pb_tx_folder, &star_btn_folder);
        }
    });

    let (ui_fav_tx, ui_fav_rx) = async_channel::unbounded::<()>();
    let fav_list_clone = fav_list.clone();
    let stream_list_clone = stream_list.clone();
    
    let st_rend = player_state.clone();
    let pb_rend = play_btn.clone();
    let t_rend = title_label.clone();
    let v_rend = vol_slider.clone();
    let prog_rend = progress_adj.clone();
    let lbl_l_rend = time_lbl_left.clone();
    let lbl_r_rend = time_lbl_right.clone();
    let cov_rend = cover_picture.clone();
    let hl_rend = history_list.clone();
    let pb_tx_rend = pb_tx.clone();
    let star_btn_rend = star_btn.clone();
    
    glib::spawn_future_local(async move {
        let favs = load_favorites();
        render_favorites_list(&fav_list_clone, favs.local, true, &st_rend, &pb_rend, &t_rend, &v_rend, &prog_rend, &lbl_l_rend, &lbl_r_rend, &cov_rend, &hl_rend, &pb_tx_rend, &star_btn_rend);
        render_favorites_list(&stream_list_clone, favs.streaming, false, &st_rend, &pb_rend, &t_rend, &v_rend, &prog_rend, &lbl_l_rend, &lbl_r_rend, &cov_rend, &hl_rend, &pb_tx_rend, &star_btn_rend);
        
        while let Ok(_) = ui_fav_rx.recv().await {
            let favs = load_favorites();
            render_favorites_list(&fav_list_clone, favs.local, true, &st_rend, &pb_rend, &t_rend, &v_rend, &prog_rend, &lbl_l_rend, &lbl_r_rend, &cov_rend, &hl_rend, &pb_tx_rend, &star_btn_rend);
            render_favorites_list(&stream_list_clone, favs.streaming, false, &st_rend, &pb_rend, &t_rend, &v_rend, &prog_rend, &lbl_l_rend, &lbl_r_rend, &cov_rend, &hl_rend, &pb_tx_rend, &star_btn_rend);
        }
    });

    let st_next = player_state.clone();
    let play_btn_n = play_btn.clone();
    let title_n = title_label.clone();
    let vol_n = vol_slider.clone();
    let prog_n = progress_adj.clone();
    let lbl_l_n = time_lbl_left.clone();
    let lbl_r_n = time_lbl_right.clone();
    let cover_n = cover_picture.clone();
    let hist_list_n = history_list.clone();
    let pb_tx_n = pb_tx.clone();
    let star_btn_n = star_btn.clone();
    
    next_btn.connect_clicked(move |_| {
        let (track, idx) = {
            let st = st_next.borrow();
            if st.tracks.is_empty() { return; }
            let new_idx = (st.current_idx + 1) % st.tracks.len();
            (st.tracks[new_idx].clone(), new_idx)
        };
        play_track_obj(&st_next, track, Some(idx), &play_btn_n, &title_n, &vol_n, &prog_n, &lbl_l_n, &lbl_r_n, &cover_n, &hist_list_n, &pb_tx_n, &star_btn_n);
    });

    let st_prev = player_state.clone();
    let play_btn_p = play_btn.clone();
    let title_p = title_label.clone();
    let vol_p = vol_slider.clone();
    let prog_p = progress_adj.clone();
    let lbl_l_p = time_lbl_left.clone();
    let lbl_r_p = time_lbl_right.clone();
    let cover_p = cover_picture.clone();
    let hist_list_p = history_list.clone();
    let pb_tx_p = pb_tx.clone();
    let star_btn_p = star_btn.clone();
    
    prev_btn.connect_clicked(move |_| {
        let (track, idx) = {
            let st = st_prev.borrow();
            if st.tracks.is_empty() { return; }
            let new_idx = if st.current_idx == 0 { st.tracks.len() - 1 } else { st.current_idx - 1 };
            (st.tracks[new_idx].clone(), new_idx)
        };
        play_track_obj(&st_prev, track, Some(idx), &play_btn_p, &title_p, &vol_p, &prog_p, &lbl_l_p, &lbl_r_p, &cover_p, &hist_list_p, &pb_tx_p, &star_btn_p);
    });

    let (sender, receiver) = async_channel::unbounded::<Vec<Track>>();
    let results_container_clone = results_container.clone();
    
    let st_search_res = player_state.clone();
    let play_btn_res = play_btn.clone();
    let title_res = title_label.clone();
    let vol_res = vol_slider.clone();
    let prog_adj_res = progress_adj.clone();
    let lbl_l_res = time_lbl_left.clone();
    let lbl_r_res = time_lbl_right.clone();
    let cover_res = cover_picture.clone();
    let hist_list_res = history_list.clone();
    let pb_tx_res = pb_tx.clone();
    let star_btn_res = star_btn.clone();
    
    let search_popover_res = search_popover.clone();
    
    glib::spawn_future_local(async move {
        while let Ok(tracks) = receiver.recv().await {
            st_search_res.borrow_mut().tracks = tracks.clone();
            while let Some(child) = results_container_clone.first_child() { results_container_clone.remove(&child); }
            
            if tracks.is_empty() {
                results_container_clone.append(&Label::builder().label("Ничего не найдено").margin_top(10).margin_bottom(10).build());
            } else {
                for (i, track) in tracks.iter().enumerate() {
                    let title = track.title.clone().unwrap_or_else(|| "Неизвестно".into());
                    let is_stream = track.get_url().starts_with("http");
                    
                    let item_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).build();
                    let icon_name = if is_stream { "network-wireless-symbolic" } else { "audio-x-generic-symbolic" };
                    let source_icon = Image::builder().icon_name(icon_name).css_classes(["muted-icon"]).build();
                    let title_lbl = Label::builder().label(&title).halign(Align::Start).ellipsize(gtk4::pango::EllipsizeMode::End).hexpand(true).build();
                    
                    let source_text = track.platform.clone().unwrap_or_else(|| if is_stream { "YouTube".to_string() } else { "Local".to_string() });
                    let source_lbl = Label::builder().label(&source_text).css_classes(["history-platform"]).valign(Align::Center).build();

                    item_box.append(&source_icon);
                    item_box.append(&title_lbl);
                    item_box.append(&source_lbl);
                    
                    let btn = Button::builder().css_classes(["track-result-btn"]).child(&item_box).build();
                    
                    let st_click = st_search_res.clone();
                    let track_clone = track.clone();
                    let btn_click = play_btn_res.clone();
                    let title_click = title_res.clone();
                    let vol_click = vol_res.clone();
                    let prog_click = prog_adj_res.clone();
                    let lbl_l_click = lbl_l_res.clone();
                    let lbl_r_click = lbl_r_res.clone();
                    let cover_click = cover_res.clone();
                    let hist_click = hist_list_res.clone();
                    let pb_tx_click = pb_tx.clone();
                    let star_btn_click = star_btn_res.clone();
                    let popover_click = search_popover_res.clone();
                    
                    btn.connect_clicked(move |_| {
                        popover_click.popdown();
                        play_track_obj(&st_click, track_clone.clone(), Some(i), &btn_click, &title_click, &vol_click, &prog_click, &lbl_l_click, &lbl_r_click, &cover_click, &hist_click, &pb_tx_click, &star_btn_click);
                    });
                    
                    results_container_clone.append(&btn);
                }
            }
        }
    });

    let search_entry_clone = search_entry.clone();
    let results_container_clone2 = results_container.clone();
    let source_dropdown_clone = source_dropdown.clone(); 
    
    let search_popover_act = search_popover.clone();
    
    search_entry.connect_activate(move |_| {
        let query = search_entry_clone.text().to_string();
        if query.trim().is_empty() { return; }
        
        let selected_platform = if let Some(obj) = source_dropdown_clone.selected_item() {
            if let Ok(str_obj) = obj.downcast::<StringObject>() {
                str_obj.string().to_string()
            } else { "YouTube".to_string() }
        } else { "YouTube".to_string() };

        while let Some(child) = results_container_clone2.first_child() { results_container_clone2.remove(&child); }
        results_container_clone2.append(&Label::builder().label(&format!("Ищем в {}...", selected_platform)).margin_top(10).margin_bottom(10).build());
        
        search_popover_act.popup();
        
        let s = sender.clone();
        std::thread::spawn(move || {
            let mut tracks = vec![];
            let dir = get_app_dir();
            let query_lower = query.to_lowercase();
            
            if let Ok(entries) = std::fs::read_dir(&dir) {
                for entry in entries.flatten() {
                    if let Ok(name) = entry.file_name().into_string() {
                        if name.to_lowercase().contains(&query_lower) && name.ends_with(".mp3") {
                            tracks.push(Track {
                                title: Some(name.replace(".mp3", "")),
                                url: Some(entry.path().to_string_lossy().into_owned()),
                                webpage_url: None, duration: Some(0.0), thumbnail: None, thumbnails: None,
                                platform: Some("Local".to_string()),
                            });
                        }
                    }
                }
            }
            
            let search_query = match selected_platform.as_str() {
                "Spotify" => format!("ytsearch5:{} spotify", query),
                _ => format!("ytsearch5:{}", query),
            };

            let output = Command::new("yt-dlp")
                .args(["--dump-json", "--default-search", "ytsearch", "--flat-playlist", &search_query])
                .output();
                
            if let Ok(out) = output {
                let stdout = String::from_utf8_lossy(&out.stdout);
                for line in stdout.lines() {
                    if let Ok(mut t) = serde_json::from_str::<Track>(line) { 
                        t.platform = Some(selected_platform.clone());
                        tracks.push(t); 
                    }
                }
            }
            let _ = s.send_blocking(tracks);
        });
    });

    let bottom_bar = CenterBox::builder().css_classes(["bottom-bar"]).margin_start(15).margin_end(15).margin_bottom(15).build();
    let b_left = GtkBox::builder().orientation(Orientation::Horizontal).spacing(10).margin_start(20).build();
    
    b_left.append(&star_btn);
    b_left.append(&dl_btn);
    bottom_bar.set_start_widget(Some(&b_left));

    let st_star = player_state.clone();
    let ui_fav_tx_clone = ui_fav_tx.clone();
    star_btn.connect_clicked(move |btn| {
        if let Some(track) = &st_star.borrow().current_track {
            let mut favs = load_favorites();
            let is_streaming = track.get_url().starts_with("http");
            let mut removed = false;
            
            if is_streaming {
                if let Some(pos) = favs.streaming.iter().position(|t| t.get_url() == track.get_url()) {
                    favs.streaming.remove(pos);
                    removed = true;
                } else {
                    favs.streaming.push(track.clone());
                }
            } else {
                if let Some(pos) = favs.local.iter().position(|t| t.get_url() == track.get_url()) {
                    favs.local.remove(pos);
                    removed = true;
                } else {
                    favs.local.push(track.clone());
                }
            }
            save_favorites(&favs);
            btn.set_icon_name(if removed { "non-starred-symbolic" } else { "starred-symbolic" });
            let _ = ui_fav_tx_clone.send_blocking(());
        }
    });

    let st_dl = player_state.clone();
    let dl_btn_clone = dl_btn.clone();
    dl_btn.connect_clicked(move |_| {
        if let Some(track) = &st_dl.borrow().current_track {
            let url = track.get_url();
            if url.starts_with("http") {
                let dir = get_app_dir();
                
                dl_btn_clone.set_icon_name("emblem-ok-symbolic");
                let btn_reset = dl_btn_clone.clone();
                glib::timeout_add_local(std::time::Duration::from_millis(2000), move || {
                    btn_reset.set_icon_name("go-down-symbolic");
                    glib::ControlFlow::Break
                });
                
                std::thread::spawn(move || {
                    let _ = Command::new("yt-dlp")
                        .args(["-x", "--audio-format", "mp3", "-o", &format!("{}/%(title)s.%(ext)s", dir.display()), &url])
                        .output();
                });
            }
        }
    });

    let b_center = GtkBox::builder().orientation(Orientation::Vertical).valign(Align::Center).build();
    let b_controls = GtkBox::builder().orientation(Orientation::Horizontal).spacing(15).halign(Align::Center).build();
    b_controls.append(&prev_btn);
    b_controls.append(&play_btn);
    b_controls.append(&next_btn);
    
    let progress_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(10).halign(Align::Center).build();
    progress_box.append(&time_lbl_left);
    progress_box.append(&progress);
    progress_box.append(&time_lbl_right);

    b_center.append(&b_controls);
    b_center.append(&progress_box);
    bottom_bar.set_center_widget(Some(&b_center));

    let b_right = GtkBox::builder().orientation(Orientation::Horizontal).spacing(10).margin_end(20).valign(Align::Center).build();
    b_right.append(&vol_slider);
    b_right.append(&Button::builder().icon_name("audio-volume-high-symbolic").css_classes(["player-btn"]).build());
    bottom_bar.set_end_widget(Some(&b_right));

    main_vbox.append(&bottom_bar);

    let conf = load_config();
    
    let wallpaper_provider = CssProvider::new();
    gtk4::style_context_add_provider_for_display(
        &Display::default().unwrap(),
        &wallpaper_provider,
        gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION + 1,
    );
    update_wallpaper(conf.custom_wallpapers, &conf.wallpapers_path, &wallpaper_provider);

    let settings_menu = GtkBox::builder().orientation(Orientation::Vertical).css_classes(["settings-menu"]).build();
    settings_menu.append(&Label::builder().label("Settings").css_classes(["panel-header"]).halign(Align::Center).margin_bottom(15).build());

    let row1 = GtkBox::builder().orientation(Orientation::Horizontal).spacing(15).margin_bottom(10).build();
    row1.append(&Label::builder().label("Auto song download").halign(Align::Start).hexpand(true).build());
    let sw_auto = Switch::builder().active(conf.auto_download).valign(Align::Center).build();
    row1.append(&sw_auto);
    settings_menu.append(&row1);

    sw_auto.connect_active_notify(|sw| {
        let mut c = load_config();
        c.auto_download = sw.is_active();
        save_config(&c);
    });

    let row2 = GtkBox::builder().orientation(Orientation::Horizontal).spacing(15).margin_bottom(15).build();
    row2.append(&Label::builder().label("Enable custom wallpapers").halign(Align::Start).hexpand(true).build());
    let sw_wall = Switch::builder().active(conf.custom_wallpapers).valign(Align::Center).build();
    row2.append(&sw_wall);
    settings_menu.append(&row2);

    let wp_prov_clone = wallpaper_provider.clone();
    sw_wall.connect_active_notify(move |sw| {
        let mut c = load_config();
        c.custom_wallpapers = sw.is_active();
        save_config(&c);
        update_wallpaper(c.custom_wallpapers, &c.wallpapers_path, &wp_prov_clone);
    });

    let row3 = GtkBox::builder().orientation(Orientation::Vertical).spacing(5).margin_bottom(20).build();
    row3.append(&Label::builder().label("Wallpapers path").halign(Align::Start).build());
    let entry_path = Entry::builder().placeholder_text("/home/yatavo/Pictures/bg.jpg").text(&conf.wallpapers_path).build();
    row3.append(&entry_path);
    settings_menu.append(&row3);

    let wp_prov_clone2 = wallpaper_provider.clone();
    entry_path.connect_changed(move |entry| {
        let mut c = load_config();
        c.wallpapers_path = entry.text().to_string();
        save_config(&c);
        update_wallpaper(c.custom_wallpapers, &c.wallpapers_path, &wp_prov_clone2);
    });

    settings_menu.append(&Label::builder().label("YaPlayer made by YATAVO <3").css_classes(["muted-text"]).halign(Align::Center).build());
    
    let github_link = LinkButton::builder()
        .uri("https://github.com/YATAVO")
        .label("github.com/YATAVO")
        .css_classes(["muted-text"])
        .halign(Align::Center)
        .build();
    settings_menu.append(&github_link);
    
    let settings_popover = Popover::builder().child(&settings_menu).build();
    settings_btn.set_popover(Some(&settings_popover));

    let eq_menu = GtkBox::builder().orientation(Orientation::Vertical).css_classes(["settings-menu"]).build();
    eq_menu.append(&Label::builder().label("Equalizer (10-Band)").css_classes(["panel-header"]).halign(Align::Center).margin_bottom(15).build());

    let eq_hbox = GtkBox::builder().orientation(Orientation::Horizontal).spacing(10).halign(Align::Center).build();
    
    let labels = ["31", "62", "125", "250", "500", "1K", "2K", "4K", "8K", "16K"];
    let mut eq_adjs = vec![];

    for (i, &lbl) in labels.iter().enumerate() {
        let vbox = GtkBox::builder().orientation(Orientation::Vertical).spacing(5).build();
        let adj = Adjustment::new(conf.eq_bands[i], -15.0, 15.0, 1.0, 5.0, 0.0);
        let scale = Scale::builder().orientation(Orientation::Vertical).adjustment(&adj).inverted(true).height_request(130).build();
        scale.set_draw_value(false);
        
        let label_widget = Label::builder().label(lbl).css_classes(["muted-text"]).halign(Align::Center).build();
        
        vbox.append(&scale);
        vbox.append(&label_widget);
        eq_hbox.append(&vbox);
        eq_adjs.push(adj);
    }
    eq_menu.append(&eq_hbox);
    
    let btn_reset = Button::builder().label("Reset").margin_top(15).css_classes(["track-result-btn"]).build();
    eq_menu.append(&btn_reset);

    let adjs_rc = Rc::new(eq_adjs);
    for i in 0..10 {
        let adjs_clone = adjs_rc.clone();
        adjs_rc[i].connect_value_changed(move |_| {
            let mut c = load_config();
            for j in 0..10 {
                c.eq_bands[j] = adjs_clone[j].value();
            }
            save_config(&c);
            
            if let Ok(mut stream) = UnixStream::connect("/tmp/yaplayer_ipc.sock") {
                let af = build_eq_af(&c.eq_bands);
                let cmd = format!("{{\"command\": [\"set_property\", \"af\", \"{}\"]}}\n", af);
                let _ = stream.write_all(cmd.as_bytes());
            }
        });
    }

    let adjs_reset = adjs_rc.clone();
    btn_reset.connect_clicked(move |_| {
        for adj in adjs_reset.iter() {
            adj.set_value(0.0);
        }
    });

    let eq_popover = Popover::builder().child(&eq_menu).build();
    eq_btn.set_popover(Some(&eq_popover));

    let window = ApplicationWindow::builder().application(app).title("Yatavo Player").default_width(1200).default_height(750).child(&overlay).css_classes(["app-background"]).build();
    
    let st_close = player_state.clone();
    window.connect_close_request(move |_| {
        if let Some(mut p) = st_close.borrow_mut().process.take() {
            let _ = p.kill();
            let _ = p.wait();
        }
        glib::Propagation::Proceed
    });

    window.present();
}

fn create_list_panel(icon_name: &str, title: &str) -> (GtkBox, GtkBox) {
    let panel = GtkBox::builder().orientation(Orientation::Vertical).css_classes(["panel-bg"]).vexpand(true).hexpand(false).width_request(280).build();
    
    let header_box = GtkBox::builder().orientation(Orientation::Horizontal).spacing(8).margin_top(15).margin_bottom(10).halign(Align::Center).build();
    header_box.append(&Image::builder().icon_name(icon_name).build());
    header_box.append(&Label::builder().label(title).css_classes(["panel-header"]).build());
    panel.append(&header_box);
    
    let scroll = ScrolledWindow::builder().vexpand(true).hexpand(false).hscrollbar_policy(PolicyType::Never).build();
    let list_box = GtkBox::builder().orientation(Orientation::Vertical).hexpand(false).spacing(2).build();
    
    scroll.set_child(Some(&list_box));
    panel.append(&scroll);
    (panel, list_box)
}

fn load_css() {
    let provider = CssProvider::new();
    provider.load_from_data(
        "
        .app-background { transition: background-image 0.3s ease-in-out; }
        .panel-bg { background-color: alpha(currentColor, 0.05); border-radius: 15px; }
        .panel-header { font-weight: bold; font-size: 14px; }
        
        .icon-btn { background-color: alpha(currentColor, 0.1); color: currentColor; border-radius: 12px; padding: 8px 12px; border: none; }
        .icon-btn:hover { background-color: alpha(currentColor, 0.15); }
        
        menubutton.menu-btn > button {
            background-color: alpha(currentColor, 0.1);
            color: currentColor;
            border-radius: 12px;
            padding: 8px 12px;
            border: none;
        }
        menubutton.menu-btn > button:hover {
            background-color: alpha(currentColor, 0.15);
        }
        menubutton.menu-btn arrow {
            opacity: 0; min-width: 0; min-height: 0; padding: 0; margin: 0; border: none;
        }
        
        .search-dropdown { background: transparent; border: none; box-shadow: none; font-weight: bold; padding: 5px; }
        
        .search-bar { background-color: alpha(currentColor, 0.05); border-radius: 22px; padding: 5px 15px; }
        .search-entry { background: transparent; border: none; box-shadow: none; color: currentColor; }
        .muted-icon { color: alpha(currentColor, 0.5); }
        .muted-text { color: alpha(currentColor, 0.5); font-size: 11px; }
        
        .cover-art-pic { background-color: alpha(currentColor, 0.08); border-radius: 24px; min-height: 400px; min-width: 400px; }
        .history-avatar { background-color: alpha(currentColor, 0.08); border-radius: 6px; }
        
        .song-title { font-size: 32px; font-weight: bold; }
        .song-artist { font-size: 18px; color: alpha(currentColor, 0.6); }
        
        .history-title { font-size: 12px; }
        .history-platform { font-size: 10px; font-weight: bold; background-color: alpha(currentColor, 0.1); padding: 2px 6px; border-radius: 6px; color: alpha(currentColor, 0.7); margin-right: 5px; }
        .time-label { font-size: 12px; color: alpha(currentColor, 0.7); min-width: 40px; }
        
        .bottom-bar { background-color: alpha(currentColor, 0.08); border-radius: 15px; padding: 10px; min-height: 85px; }
        .flat-icon-btn { background: transparent; color: currentColor; border: none; font-size: 20px; }
        .player-btn { background-color: alpha(currentColor, 0.12); color: currentColor; border-radius: 18px; border: none; padding: 10px; }
        .player-btn:hover { background-color: alpha(currentColor, 0.2); }
        
        .player-btn-main { background-color: @theme_fg_color; color: @theme_bg_color; border-radius: 22px; border: none; padding: 14px; }
        .player-btn-main:hover { opacity: 0.8; }
        
        .settings-menu { padding: 15px; min-width: 280px; }
        
        .results-container { padding: 5px; min-width: 400px; }
        
        .track-result-btn, .history-item-btn { 
            background-color: transparent; 
            color: currentColor; 
            border: none; 
            border-radius: 8px; 
            padding: 4px; 
        }
        .track-result-btn { padding: 10px; font-size: 14px; }
        .history-item-btn:hover, .track-result-btn:hover { background-color: alpha(currentColor, 0.08); }
        "
    );
    gtk4::style_context_add_provider_for_display(&Display::default().expect("Could not connect to a display."), &provider, gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION);
}