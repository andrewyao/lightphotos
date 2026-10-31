// SPDX-License-Identifier: MIT OR Apache-2.0

//! German.

use super::*;

pub static DE: Strings = Strings {
    settings: "Einstellungen",
    settings_tip: if SHOW_AUTOTONE_CENTERING && RAW_SETTING {
        "Design, Sprache, RAW-Tonwertkurve und Auto-Anpassung (Cmd+,)"
    } else if SHOW_AUTOTONE_CENTERING {
        "Design, Sprache und Auto-Anpassung (Cmd+,)"
    } else if RAW_SETTING {
        "Design, Sprache und RAW-Tonwertkurve (Cmd+,)"
    } else {
        "Design und Sprache (Cmd+,)"
    },
    settings_title: "Einstellungen",
    settings_theme: "Design",
    settings_language: "Sprache",
    settings_auto_tone: "Auto-Anpassung",
    settings_raw: "RAW-Dateien",
    apple_tone_curve: "Apple-Tonwertkurve",
    apple_tone_curve_hint: "Eingeschaltet sehen RAW-Dateien so aus wie in Fotos und Preview. Ausgeschaltet verwendet LightPhotos eine eigene Kurve, wie im Browser.",
    autotone_center_range: "Bereich zentrieren",
    autotone_center_range_tip: "Die Mitte des Histogrammbereichs auf 50% legen. Die Form des Fotos bleibt erhalten.",
    autotone_center_median: "Mittleres Pixel zentrieren",
    autotone_center_median_tip: "Das Pixel, das heller ist als die dunklere Hälfte des Fotos, auf 50% legen. Ein überwiegend dunkles Foto wird heller.",
    form_general: "Allgemein",
    theme_dark: "Dunkel",
    theme_medium: "Mittel",
    theme_light: "Hell",

    open_folder: "Ordner öffnen",
    open_folder_tip: "Einen Ordner mit Fotos zu „Ordner“ hinzufügen und öffnen (Cmd+O)",
    back_to_grid_tip: "Zurück zum Raster (G)",
    fullscreen_tip: "Vollbild (F)",
    slideshow_menu_tip: "Optionen für die Diashow",
    slideshow_play: "Diashow starten (Cmd+Enter)",
    slideshow_duration: "Anzeigedauer",
    slideshow_random: "Zufällig",
    slideshow_fill: "Bildfüllend zoomen",
    seconds_suffix: " s",
    shown_thumb_tip: "Zeigt die Miniatur",
    shown_embedded_tip: "Zeigt das in die Datei eingebettete JPEG",
    shown_preview_tip: "Zeigt eine Vorschau unterhalb der vollen Auflösung",
    shown_full_tip: "Zeigt die volle Auflösung",
    help_tip: "Tastaturkurzbefehle (?)",
    metadata_title: "Metadaten",
    info_tab_tip: "Metadaten des Fotos (I)",
    info_no_selection: "Kein Foto ausgewählt",
    info_file: "Datei",
    info_camera: "Kamera",
    info_exposure: "Belichtung",
    info_date: "Datum",
    info_location: "Ort",
    info_name: "Name",
    info_dimensions: "Abmessungen",
    info_modified: "Geändert",
    info_make: "Hersteller",
    info_model: "Modell",
    info_lens: "Objektiv",
    info_shutter: "Verschlusszeit",
    info_aperture: "Blende",
    info_iso: "ISO",
    info_focal_length: "Brennweite",
    info_exposure_comp: "Belichtungskorr.",
    info_flash: "Blitz",
    info_white_balance: "Weißabgleich",
    info_captured: "Aufgenommen",
    info_latitude: "Breitengrad",
    info_longitude: "Längengrad",
    info_altitude: "Höhe",
    info_open_in_maps: "In Karten öffnen",
    flash_fired: "Ausgelöst",
    flash_did_not_fire: "Nicht ausgelöst",
    wb_auto: "Auto",
    wb_manual: "Manuell",
    landing_prompt: "Wählen Sie einen Ordner mit Fotos, um zu beginnen",
    landing_tagline: "Fotos schnell aussortieren, bewerten und entwickeln.",
    landing_allow_note: if WEB {
        "Wenn der Browser fragt „Diese Website darf Dateien bearbeiten? \u{2026}“, klicken Sie auf „Zulassen“.\n\
         So kann die Anwendung Bewertungen und Bearbeitungen in einem Unterordner „.lightphotos“ sichern.\n\
         Ihre Originalfotos werden nie verändert."
    } else {
        ""
    },
    opening: "Wird geöffnet\u{2026}",
    picker_title: "Ordner mit Fotos wählen",
    add_folder: "Ordner hinzufügen",
    add_folder_tip: "Einen Ordner mit Fotos hinzufügen (Cmd+O)",
    remove_folder: "Ordner aus LightPhotos entfernen",
    folder_missing: |folder| format!("{folder} wurde nicht gefunden. Verbinden Sie das Laufwerk erneut oder entfernen Sie den Ordner per Rechtsklick."),
    folder_not_kept: |folder| format!("Dieser Browser hat keinen Zugriff mehr auf {folder}. Fügen Sie ihn erneut hinzu oder entfernen Sie ihn per Rechtsklick."),
    select_photos_hint: "Wählen Sie Fotos aus, um sie hier zu bewerten, zu stapeln und zu exportieren.",
    tour: "Rundgang",
    tour_tip: "Ein kurzer Rundgang durch die App",
    take_tour: "Rundgang starten",
    skip_tour: "Rundgang überspringen",
    tour_back: "Zurück",
    tour_next: "Weiter",
    tour_done: "Fertig",
    tour_step_of: |n, m| format!("{n} von {m}"),
    tour_loupe_icons: [
        "Das Foto zuschneiden, drehen und begradigen. Zum Öffnen R drücken.",
        "Über Flecken und Staub pinseln, um sie zu entfernen. Zum Öffnen Q drücken.",
        "Bei einem Foto in einem Stapel die Aufnahmen des Stapels nebeneinander zeigen, um die beste zu wählen.",
    ],
    tour_steps: [
        (
            "Ordner",
            "„Ordner öffnen“ fügt hier einen Ordner mit Fotos hinzu, „Ordner hinzufügen“ über der Liste einen weiteren. Jeder wird mit seinen Unterordnern aufgeführt. Ein Klick zeigt seine Fotos; per Rechtsklick lässt sich ein Ordner aus der Liste entfernen.",
        ),
        (
            "Filterleiste",
            "Nur Fotos mit einer Bewertung, Markierung oder Farbmarkierung zeigen und die Sortierung ändern.",
        ),
        (
            "Fotos",
            "Miniaturen des Ordners. Doppelklicken Sie auf ein Foto oder drücken Sie Enter, um es in der Lupenansicht zu öffnen. Per Rechtsklick gibt es mehr.",
        ),
        (
            "Aktionsleiste",
            "Wirkt auf die Auswahl: bewerten, Anpassungen kopieren oder anwenden. Im Menü „Aktionen“ lassen sich Serienbilder stapeln, automatisch anpassen oder löschen.",
        ),
        (
            "Anpassungen",
            "Das Regler-Symbol öffnet für das ausgewählte Foto die Regler für Tonwerte und Farben sowie Ihre gesicherten Vorgaben.",
        ),
        (
            "Info",
            "Das i-Symbol zeigt die Metadaten des Fotos: Kamera, Objektiv, Belichtung sowie wann und wo es aufgenommen wurde. Mit I lässt sich der Bereich ebenfalls zeigen.",
        ),
        (
            "Export",
            "Das Export-Symbol wählt einen Ordner und eine Größe und exportiert die ausgewählten Fotos als JPEGs. Mit Cmd+Shift+E öffnen Sie es ebenfalls.",
        ),
        (
            "Mehr in der Lupenansicht",
            "Bei geöffnetem Foto kommen in der Leiste drei weitere Symbole hinzu:",
        ),
        (
            "Kopfleiste",
            "Die Taste ? listet die Tastaturkurzbefehle auf, in den Einstellungen ändern Sie Design, Sprache und Textgröße, und „Rundgang“ zeigt diese Anleitung erneut.",
        ),
    ],


    rating_filter: "Bewertung:",
    all: "Alle",
    at_least_n_stars: "Mindestens N Sterne",
    exactly_n_stars: "Genau N Sterne",
    at_most_n_stars: "Höchstens N Sterne",
    show_rated: |cmp, n| format!("Fotos mit Bewertung {cmp} {n} zeigen"),
    unrated_tip: "Nur Fotos ohne Bewertung zeigen",
    flag_filter_label: "Markierung:",
    show_flagged: |flag| format!("Nur Fotos zeigen: {flag}"),
    label_filter_label: "Farbe:",
    show_labeled: |label| format!("Fotos mit Farbmarkierung {label} zeigen"),
    flag_not_rejected: "Markiert oder unmarkiert",
    flag_picked: "Markiert",
    flag_rejected: "Abgelehnt",
    flag_unflagged: "Unmarkiert",
    set_flag_tip: |flag| format!("Markierung setzen: {flag}"),
    eyes_closed: "Augen geschlossen",
    eyes_closed_tip: "Nur Fotos zeigen, auf denen jemand geblinzelt hat",
    n_of_m_photos: |n, m| format!("{n} / {m} Fotos"),
    no_filter_matches: "Kein Foto entspricht dem Filter",
    show_all_photos: "Alle Fotos zeigen",
    thumb_unreadable: "Nicht lesbar",

    selected_photo: "Ausgewähltes Foto:",
    selected_n_photos: |n| format!("{n} Fotos ausgewählt:"),
    score_all: "Alle auswerten",
    score_all_tip: "Die Qualität jedes Fotos hier von 0 bis 100 auswerten, auch die Fotos in Stapeln",
    group_all_bursts: "Auto-Stapel nach Aufnahmezeit",
    group_all_bursts_tip: "Fotos hier stapeln, die innerhalb einer Sekunde nacheinander aufgenommen wurden",
    group_selected_bursts_tip: "Ausgewählte Fotos stapeln, die innerhalb einer Sekunde nacheinander aufgenommen wurden",
    expand_stack: "Stapel erweitern",
    collapse_stack: "Stapel reduzieren",
    auto_adjust_all: "Alle automatisch anpassen",
    auto_adjust_all_tip: "Die Tonwertregler jedes Fotos anhand seines eigenen Histogramms setzen, auch bei Fotos in Stapeln",
    update_score: "Neu auswerten",
    n_selected: |n| format!("{n} ausgewählt"),
    rate_menu: "Bewerten \u{2605}",
    actions_menu: "Aktionen",
    open_in_loupe: "In Lupenansicht öffnen",
    group_into_stack: "Stapeln",
    reset_adjustments: "Anpassungen zurücksetzen",
    score_label: "Qualität:",
    selected_n_photos_title: |n| format!("{n} Fotos ausgewählt"),
    update_scores: "Neu auswerten",
    clear_rating: "Bewertung entfernen",
    auto_tone: "Auto-Anpassung",
    auto_tone_selection_tip:
        "Die Tonwertregler jedes ausgewählten Fotos anhand seines eigenen Histogramms setzen",
    score_selection_tip: "Die Qualität jedes ausgewählten Fotos von 0 bis 100 auswerten",
    quality_of: |v| format!("Qualität {v} von 100"),
    base_technical: |b| format!("Beginnt bei {b}: nur technische Prüfungen (Schärfe, Belichtung, Rauschen, Augen)"),
    base_aesthetics: |b| format!("Apple-Ästhetikwert: {b}"),
    score_no_breakdown: "Erneut auswerten, um die Aufschlüsselung zu sehen",
    no_penalties: "Keine Mängel gefunden",
    penalty: |p| {
        use crate::scoring::quality::Penalty::*;
        match p {
            SoftFocus => "Unscharf",
            HighlightsClipped => "Ausgefressene Lichter",
            ShadowsCrushed => "Abgesoffene Tiefen",
            Dark => "Unterbelichtet",
            Bright => "Überbelichtet",
            Noisy => "Verrauscht",
            EyesClosed => "Augen geschlossen",
            Utility => "Wirkt wie ein Screenshot oder Dokument",
        }
    },
    score_stale: "Seit der Auswertung bearbeitet; erneut auswerten zum Aktualisieren",
    sort_by: "Sortieren:",
    sort_name: "Name",
    sort_time: "Aufnahmezeit",
    zoom_fit: "Einpassen",
    zoom_tip: "Zoom, vom Einpassen ins Fenster bis 6400%",
    zoom_actual: "100% (1:1)",
    sort_time_tip: "Nach Aufnahmezeit. Fotos ohne Aufnahmezeit kommen zuletzt.",
    sort_modified: "Änderungszeit",
    sort_modified_tip: "Danach, wann die Datei zuletzt geändert wurde.",
    sort_size: "Dateigröße",
    sort_size_tip: "Nach Dateigröße.",
    sort_shutter: "Verschlusszeit",
    sort_shutter_tip: "Nach Verschlusszeit. Fotos ohne Verschlusszeit kommen zuletzt.",
    sort_quality: "Qualität",
    sort_quality_tip: "Nach Qualitätswert. Nicht ausgewertete Fotos kommen zuletzt.",
    sort_rating: "Bewertung",
    sort_rating_tip: "Nach Sternebewertung. Nicht bewertete Fotos zählen als ohne Sterne.",
    sort_edited: "Bearbeitungszeit",
    sort_edited_tip: "Danach, wann das Foto hier zuletzt bearbeitet wurde. Unbearbeitete Fotos kommen zuletzt.",
    sort_ascending_tip: "Aufsteigend. Klicken, um die Reihenfolge umzukehren.",
    sort_descending_tip: "Absteigend. Klicken, um die Reihenfolge umzukehren.",
    copy_settings: "Anpassung kopieren",
    copy_settings_tip: "Die Anpassung dieses Fotos kopieren, wie in den Kopieroptionen gewählt",
    copy_settings_choose_tip: "Wählen, was „Anpassung kopieren“ kopiert (Cmd+Shift+C)",
    copy_settings_prompt: "Wählen Sie, was „Anpassung kopieren“ auf andere Fotos überträgt. Zuschneiden, Begradigen und Retusche werden bei jedem Kopieren neu gewählt.",
    copy_button: "Kopieren",
    check_all: "Alle aktivieren",
    check_none: "Keine aktivieren",
    compare_too_many: "Vergleichen erfordert 2 bis 6 Fotos",
    effects: "Effekte",
    crop_section: "Zuschneiden",
    apply_settings: "Anpassung anwenden",
    apply_settings_needs_copy: "Kopieren Sie zuerst die Anpassung eines Fotos",
    apply_settings_tip: "Die kopierte Anpassung auf die Auswahl anwenden (Cmd+Shift+V)",
    settings_from: |name| format!("von {name}"),
    export_jpg: "Export",
    export_jpg_tip: "Ziel und Größe wählen, dann die Auswahl als JPGs exportieren (Cmd+Shift+E)",
    bursts_reading: "Aufnahmezeiten werden gelesen\u{2026}",
    bursts_none: "Keine Serienbilder gefunden",
    bursts_grouped: |n, photos| match n {
        1 => format!("1 Serienbild mit {photos} Fotos gestapelt"),
        n => format!("{n} Serienbilder gestapelt, {photos} Fotos"),
    },
    delete: "Löschen",
    delete_selection_tip: if WEB {
        "Ausgewählte Fotos endgültig löschen; nicht widerrufbar (Delete)"
    } else {
        "Ausgewählte Fotos in den Papierkorb legen (Delete)"
    },

    confirm: "Bestätigen",
    cancel: "Abbrechen",
    shortcuts_title: "Tastaturkurzbefehle",
    close: "Schließen",
    help: &[
        HelpSection {
            title: "Navigation",
            rows: &[
                ("Enter or Space", "Ausgewähltes Foto öffnen (im Raster)"),
                ("E", "Ausgewähltes Foto öffnen (im Raster)"),
                ("G", "Zurück zum Raster"),
                ("F", "Vollbild, nur das Foto; F oder Esc beendet (im Editor)"),
                ("Cmd+Enter", "Diashow der Fotos im Raster; F oder Esc beendet"),
                ("Esc", "Zurück zum Raster (im Editor)"),
                ("\u{2190} / \u{2192}", "Vorheriges / nächstes Foto (im Editor)"),
                ("Page Up / Page Down", "Vorheriges / nächstes Foto (im Editor)"),
                (
                    "\u{2190} \u{2192} \u{2191} \u{2193}",
                    "Auswahl im Raster bewegen",
                ),
                ("D", "Das ausgewählte Foto anpassen"),
                ("C", "2 bis 6 ausgewählte Fotos oder den ausgewählten Stapel vergleichen"),
                ("I", "Fotoinfo im rechten Bereich zeigen oder ausblenden"),
                ("F6 / Shift+F6", "Nächster Bereich oder nächstes Bedienelement im aktiven Bereich"),
                ("Tab / Shift+Tab", "Zwischen den Objekten im fokussierten Bereich wechseln"),
                ("Cmd+O", "Einen Ordner öffnen"),
                ("?", "Diese Hilfe zeigen oder ausblenden"),
                (
                    "Cmd+,",
                    if SHOW_AUTOTONE_CENTERING && RAW_SETTING {
                        "Einstellungen: Design, Sprache, RAW-Tonwertkurve und Auto-Anpassung"
                    } else if SHOW_AUTOTONE_CENTERING {
                        "Einstellungen: Design, Sprache und Auto-Anpassung"
                    } else if RAW_SETTING {
                        "Einstellungen: Design, Sprache und RAW-Tonwertkurve"
                    } else {
                        "Einstellungen: Design und Sprache"
                    },
                ),
                ("Alt+= / Alt+-", "Größerer / kleinerer Text"),
            ],
        },
        HelpSection {
            title: "Auswahl",
            rows: &[
                ("Cmd+A", "Alles im aktuellen Ordner auswählen (im Raster)"),
                ("Shift+Click", "Bereich auswählen"),
                ("Cmd+Click", "Einzelne Auswahl umschalten"),
                ("Shift+arrows", "Auswahl erweitern (Raster)"),
                ("Cmd+D", "Auswahl aufheben"),
                ("Cmd+G", "Die Auswahl stapeln"),
                ("Cmd+Shift+G", "Die ausgewählten Stapel auflösen"),
                (
                    "Shift+Delete",
                    "Die ausgewählten Stapel löschen oder auflösen und die Fotos behalten",
                ),
            ],
        },
        HelpSection {
            title: "Bewerten, markieren und Farbmarkierung",
            rows: &[
                ("0 1 2 3 4 5", "Sternebewertung 0-5 setzen"),
                ("Shift+0-5", "Bewertung setzen und zum nächsten Foto"),
                ("P", "Markieren"),
                ("X", "Ablehnen"),
                ("U", "Markierung entfernen"),
                ("`", "Markieren oder bei einem markierten Foto die Markierung entfernen"),
                ("6 7 8 9", "Rote, gelbe, grüne oder blaue Farbmarkierung; erneut zum Entfernen"),
            ],
        },
        HelpSection {
            title: "Zoomen und Verschieben (im Editor)",
            rows: &[
                ("Space or Z", "Zoom wechseln: einpassen, 2x eingepasst, 100%"),
                ("Cmd++ or Cmd+=", "Vergrößern (in 20%-Schritten)"),
                ("Cmd+-", "Verkleinern (in 20%-Schritten)"),
                ("\u{2191} / \u{2193}", "Vergrößern / verkleinern (in 10%-Schritten)"),
                ("Scroll", "Am Zeiger zoomen"),
                ("Drag", "Verschieben"),
                ("Space+Drag", "Verschieben"),
                ("Shift+Scroll", "Seitlich verschieben"),
                ("Alt+Scroll", "Nach oben und unten verschieben"),
            ],
        },
        HelpSection {
            title: "Bearbeiten",
            rows: &[
                ("Cmd+Z", "Die letzte Entwicklungsänderung widerrufen"),
                ("Cmd+[ / Cmd+]", "Bild um -90 / +90 Grad drehen"),
                ("R", "Zuschneiden"),
                ("Y", "Vorher-nachher-Vergleich (im Editor)"),
                ("M", "Maske „Linearer Verlauf“"),
                ("Shift+M", "Maske „Radialer Verlauf“"),
                ("Cmd+Shift+E", "Das Exportformular öffnen oder schließen"),
                ("Cmd+U", "Die Auswahl automatisch anpassen"),
                ("V", "Schwarzweiß oder zurück zu Farbe"),
                ("Cmd+Shift+C", "Die Anpassung dieses Fotos kopieren"),
                ("Cmd+Shift+V", "Die Anpassung auf die Auswahl einsetzen"),
                ("Double-click slider", "Den Regler zurücksetzen"),
                (
                    "Delete",
                    if WEB {
                        "Endgültig löschen; nicht widerrufbar"
                    } else {
                        "In den Papierkorb legen"
                    },
                ),
            ],
        },
        HelpSection {
            title: "Zuschneiden",
            rows: &[
                ("Enter or R", "Zuschnitt anwenden"),
                ("Esc", "Zuschnitt abbrechen"),
                ("Cmd+[ / Cmd+]", "Um -90 / +90 Grad drehen"),
            ],
        },
        HelpSection {
            title: "Repräsentativfoto wählen",
            rows: &[
                ("Click or drag the photo", "Den Fokusbereich verschieben"),
                ("Shift+scroll on the area", "Die Größe des Fokusbereichs ändern"),
                ("Drag a tile", "Den Fokusbereich verschieben"),
                ("Cmd+Click / Shift+Click a tile", "Weitere Kacheln wählen"),
            ],
        },
        HelpSection {
            title: "Motivauswahl",
            rows: if SUBJECT_KEYS {
                &[
                    ("O", "Die Motivauswahl zeigen oder ausblenden"),
                    ("Shift+O", "Die Motivauswahl umkehren"),
                ]
            } else {
                &[]
            },
        },
        HelpSection {
            title: "Pinselmaske",
            rows: &[
                ("K", "Pinsel ein / aus; malt die gewählte Pinselmaske"),
                ("Drag", "Malen; jeder Strich ist ein Widerrufsschritt"),
                ("Alt+Drag", "Radieren"),
                ("[ / ]", "Pinselgröße"),
                ("Scroll", "Pinselgröße"),
                ("Shift+[ / ]", "Weiche Kante des Pinsels"),
                ("Shift+Scroll", "Weiche Kante des Pinsels"),
                ("Esc", "Den Pinsel ablegen"),
            ],
        },
        HelpSection {
            title: "Retusche",
            rows: &[
                ("Q", "Retusche ein / aus"),
                ("[ / ]", "Pinselgröße"),
                ("Scroll", "Pinselgröße"),
                ("Shift+[ / ]", "Weiche Kante des Pinsels"),
                ("Shift+Scroll", "Weiche Kante des Pinsels"),
                ("O", "Die Flecken zeigen oder ausblenden"),
                ("Shift+O", "Den nächsten Fleck auswählen"),
                ("Delete", "Den ausgewählten Fleck löschen"),
                ("Cmd+Z", "Den letzten Fleck widerrufen"),
                ("Esc", "Retusche verlassen"),
            ],
        },
    ],

    confirm_apply_settings: |n| {
        format!(
            "Die kopierte Anpassung auf {n} {} anwenden?",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    confirm_auto_tone: |n| {
        format!(
            "{n} {} automatisch anpassen?",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    confirm_delete: if WEB {
        |n| {
            format!(
                "{n} {} endgültig löschen? Dies kann nicht widerrufen werden.",
                if n == 1 { "Foto" } else { "Fotos" }
            )
        }
    } else {
        |n| {
            format!(
                "{n} {} in den Papierkorb legen?",
                if n == 1 { "Foto" } else { "Fotos" }
            )
        }
    },
    confirm_delete_groups: if WEB {
        |n, g| {
            format!(
                "{n} {} endgültig löschen? Dies schließt alle Fotos in {g} {} ein. Dies kann nicht widerrufen werden.",
                if n == 1 { "Foto" } else { "Fotos" },
                if g == 1 { "Stapel" } else { "Stapeln" }
            )
        }
    } else {
        |n, g| {
            format!(
                "{n} {} in den Papierkorb legen? Dies schließt alle Fotos in {g} {} ein.",
                if n == 1 { "Foto" } else { "Fotos" },
                if g == 1 { "Stapel" } else { "Stapeln" }
            )
        }
    },
    delete_group_title: "Stapel löschen",
    delete_group_prompt: |n, g| {
        let fotos = if n == 1 { "Foto" } else { "Fotos" };
        if g == 1 {
            format!("Dieser Stapel enthält {n} {fotos}. Beim Auflösen bleiben sie als Einzelfotos erhalten.")
        } else {
            format!("Diese {g} Stapel enthalten {n} {fotos}. Beim Auflösen bleiben sie als Einzelfotos erhalten.")
        }
    },
    remove_group: "Stapel auflösen",
    trash_group_photos: if WEB {
        |n| format!("{n} Fotos löschen")
    } else {
        |n| format!("{n} Fotos in den Papierkorb legen")
    },
    bulk_delete: if WEB { "Fotos löschen" } else { "In den Papierkorb" },

    develop: "Entwickeln",
    tab_sliders: "Anpassungen",
    tab_crop: "Zuschneiden & Transformieren",
    tab_masks: "Masken & Retusche",
    crop_rotate: "Drehen",
    crop_left: "Links",
    crop_right: "Rechts",
    crop_size: "Größe",
    crop_overlay: "Overlay",
    crop_thirds: "Drittel",
    crop_grid: "Raster",
    crop_golden: "Goldener Schnitt",
    crop_diagonal: "Diagonal",
    crop_spiral: "Spirale",
    crop_overlay_none: "Ohne",
    crop_aspect: "Format",
    crop_original: "Original",
    crop_custom: "Benutzerdefiniert",
    crop_orientation: "Ausrichtung",
    crop_horizontal: "Querformat",
    crop_vertical: "Hochformat",
    crop_straighten: "Begradigen",
    crop_straighten_tool: "Begradigen-Werkzeug",
    crop_straighten_hint: "Ziehen Sie entlang einer Linie, die waagerecht oder senkrecht sein soll, und drücken Sie dann Enter.",
    presets: "Vorgaben",
    save_preset_tip: "Diese Einstellungen als Vorgabe sichern",
    import_lr_presets: "Aus Lightroom importieren…",
    import_lr_presets_tip: "Lightroom-.xmp-Vorgabendateien importieren",
    xmp_picker_title: "Lightroom-Vorgaben wählen",
    xmp_filter_name: "Lightroom-Vorgabe",
    preset_import_note: |list| format!("Lightroom-Einstellungen ohne Entsprechung hier: {list}"),
    preset_monochrome_note: "Schwarzweiß wird als Sättigung -100 angenähert",
    no_presets: "Noch keine Vorgaben",
    apply_preset_tip: "Auf die ausgewählten Fotos anwenden",
    preset_actions_tip: "Umbenennen oder löschen",
    search_presets: "Vorgaben suchen",
    my_presets: "Meine Vorgaben",
    no_matching_presets: "Keine passenden Vorgaben",
    save_current_as_preset: "Aktuelle als Vorgabe sichern…",
    add_preset_folder: "Vorgabenordner hinzufügen…",
    add_preset_folder_tip: "Die Lightroom-.xmp-Vorgaben eines Ordners auflisten, an Ort und Stelle gelesen",
    preset_folder_picker_title: "Ordner mit Lightroom-Vorgaben wählen",
    preset_folders: "Vorgabenordner",
    remove: "Entfernen",
    reading_presets: |name| format!("{name} wird gelesen…"),
    preset_folder_unavailable: |name| format!("{name} nicht lesbar"),
    preset_folder_empty: |name| format!("Keine Vorgaben in {name}"),
    preset_folder_needs_permission: |name| format!("Klicken, um den Zugriff auf {name} zu erlauben"),
    preset_folder_truncated: |name, n| format!("Nur die ersten {n} Dateien in {name} werden aufgelistet"),
    presets_skipped: |n| {
        format!(
            "{n} {} übersprungen",
            if n == 1 { "Datei" } else { "Dateien" }
        )
    },
    presets_skipped_tip: "Profile, Looks und .xmp-Dateien, die keine Entwicklungsvorgaben sind oder sich nicht lesen ließen",
    preset_folder_add_failed: |e| format!("Der Vorgabenordner konnte nicht hinzugefügt werden: {e}"),
    save_preset_title: "Vorgabe sichern",
    rename_preset_title: "Vorgabe umbenennen",
    preset_name_hint: "Name der Vorgabe",
    preset_name_label: "Name",
    save: "Sichern",
    rename: "Umbenennen",
    preset_default_name: |n| format!("Vorgabe {n}"),
    confirm_delete_preset: |name| format!("Die Vorgabe „{name}“ löschen?"),
    reset: "Zurücksetzen",
    auto_tone_tip: "Die Tonwertregler anhand des Histogramms jedes ausgewählten Fotos setzen (Cmd+U)",
    black_and_white: "S/W",
    black_and_white_tip: "Die ausgewählten Fotos in Schwarzweiß umwandeln oder zurück zu Farbe (V)",
    touch_up: "Retusche",
    brush_size: "Größe",
    brush_size_tip: "[ / ] oder das Scrollrad",
    feather: "Weiche Kante",
    feather_tip: "Shift+[ / Shift+] oder Shift+Scrollen",
    opacity: "Deckkraft",
    opacity_tip: "Wie stark der Ausschnitt das Foto abdeckt, für Flecken, die ab jetzt hinzukommen",
    spots: "Flecken",
    pick_gray: "Grau wählen",
    pick_gray_tip: "Auf ein neutralgraues Pixel im Bild klicken",
    white_balance: "Weißabgleich",
    tone: "Tonwert",
    presence: "Präsenz",
    detail: "Details",
    optics: "Optik",
    vignette: "Vignettierung",
    grain: "Körnung",
    curve: "Kurve",
    point_curve: "Punktkurve",
    curve_linear: "Linear",
    curve_medium_contrast: "Mittlerer Kontrast",
    curve_strong_contrast: "Starker Kontrast",
    curve_custom: "Benutzerdefiniert",
    curve_rgb: "RGB",
    curve_red: "Rot",
    curve_green: "Grün",
    curve_blue: "Blau",
    color_mixer: "Farbmischer",
    gray_mix: "S/W-Mischer",
    copy_color: "Farbe",
    color_grading: "Color-Grading",
    midtones: "Mitteltöne",
    global: "Global",
    blending: "Mischen",
    balance: "Abgleich",
    mix_hue: "Farbton",
    mix_luminance: "Luminanz",
    bands: [
        "Rot", "Orange", "Gelb", "Grün", "Aqua", "Blau", "Violett", "Magenta",
    ],
    remove_ca: "Chromatische Aberration entfernen",
    remove_ca_tip: "Misst die roten und blauen Säume an kontrastreichen Kanten und gleicht sie an Grün an",
    no_photo_selected: "Kein Foto ausgewählt",
    multiple_photos_selected: "Mehrere Fotos ausgewählt",
    step_big_down: "Den Wert jedes Fotos um einen großen Schritt senken",
    step_down: "Den Wert jedes Fotos um einen kleinen Schritt senken",
    step_reset: "Bei jedem ausgewählten Foto zurücksetzen",
    step_up: "Den Wert jedes Fotos um einen kleinen Schritt erhöhen",
    step_big_up: "Den Wert jedes Fotos um einen großen Schritt erhöhen",
    temp: "Temp.",
    tint: "Tonung",
    exposure: "Belichtung",
    contrast: "Kontrast",
    highlights: "Lichter",
    shadows: "Tiefen",
    whites: "Weiß",
    blacks: "Schwarz",
    texture: "Struktur",
    clarity: "Klarheit",
    dehaze: "Dunst entfernen",
    vibrance: "Dynamik",
    saturation: "Sättigung",
    denoise: "Entrauschen",
    sharpening: "Schärfen",
    sharpen_radius: "Radius",
    sharpen_detail: "Details",
    sharpen_masking: "Maskieren",
    amount: "Betrag",
    midpoint: "Mittelpunkt",
    roundness: "Rundheit",
    grain_size: "Größe",
    roughness: "Rauheit",

    before: "Vorher",
    after: "Nachher",
    invert: "Umkehren",
    invert_tip: "Den Hintergrund statt des Motivs hervorheben",
    show_selection: "Auswahl zeigen",
    selection_pending: "Auswahl\u{2026}",
    no_subject: "Kein Motiv",
    show_selection_tip: "Das Motiv umranden, das Vision in diesem Foto findet",
    capture_date: |year, month, day, hour, minute| {
        format!("{day:02}.{month:02}.{year} {hour:02}:{minute:02}")
    },


    deleting: if WEB {
        |done, total| format!("{done}/{total} werden gelöscht\u{2026}")
    } else {
        |done, total| format!("{done}/{total} werden in den Papierkorb gelegt\u{2026}")
    },
    deleted: if WEB {
        |n| {
            format!(
                "{n} {} endgültig gelöscht",
                if n == 1 { "Foto" } else { "Fotos" }
            )
        }
    } else {
        |n| {
            format!(
                "{n} {} in den Papierkorb gelegt",
                if n == 1 { "Foto" } else { "Fotos" }
            )
        }
    },
    deleted_partial: if WEB {
        |n, total, e| format!("{n}/{total} endgültig gelöscht \u{2014} letzter Fehler: {e}")
    } else {
        |n, total, e| format!("{n}/{total} in den Papierkorb gelegt \u{2014} letzter Fehler: {e}")
    },
    delete_in_progress: "Ein anderer Löschvorgang läuft noch\u{2026}",
    group_refused_loading: "Die Stapel dieses Ordners werden noch geladen. Versuchen Sie es gleich noch einmal.",
    group_name_unsaveable: "Ein Dateiname hier kann nicht in einem Stapel gesichert werden.",
    delete_no_handle: |dir| format!("Fotos konnten nicht gelöscht werden: kein Ordner-Handle für {dir}"),
    copied_settings_from: |name| format!("Anpassung von {name} kopiert"),
    applied_settings: |n| {
        format!(
            "Anpassung auf {n} {} angewendet",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    saved_preset: |name| format!("Vorgabe {name} gesichert"),
    lr_import_status: |ok, failed, first| match (ok, failed) {
        (ok, 0) => format!(
            "{ok} {} importiert",
            if ok == 1 { "Vorgabe" } else { "Vorgaben" }
        ),
        (0, _) => first.to_string(),
        (ok, failed) => format!(
            "{ok} {} importiert, {failed} übersprungen ({first})",
            if ok == 1 { "Vorgabe" } else { "Vorgaben" }
        ),
    },
    lr_import_failed: |file, reason| format!("{file} konnte nicht importiert werden: {reason}"),
    applied_preset: |name, n| {
        format!(
            "{name} auf {n} {} angewendet",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    renamed_preset: |name| format!("Vorgabe in {name} umbenannt"),
    deleted_preset: |name| format!("Vorgabe {name} gelöscht"),
    cleared_rating: |n| {
        format!(
            "Bewertung von {n} {} entfernt",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    rated: |n, stars| {
        format!(
            "{n} {} mit \u{2605}{stars} bewertet",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    flagged: |n, flag| {
        format!(
            "Markierung für {n} {}: {flag}",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    export_title: |n| {
        if n == 1 {
            "1 Foto exportieren".into()
        } else {
            format!("{n} Fotos exportieren")
        }
    },
    export_destination: "Ziel",
    export_to_folder: "Ordner",
    export_to_immich: "Immich",
    export_exports_subfolder: "Unterordner „Exports“",
    export_chosen_folder: "Anderer Ordner",
    export_choose_folder: "Wählen\u{2026}",
    export_size: "Größe",
    export_folder: "Ort",
    export_output: "Ausgabe",
    immich_account: "Konto",
    export_size_full: "Volle Auflösung",
    export_size_long_edge: |px| format!("{px} px lange Kante"),
    export_file: "Datei",
    export_format: "Format",
    export_format_tiff16: "TIFF (16 Bit)",
    export_format_webp: "WebP (verlustfrei)",
    export_limit: "Dateigröße",
    export_limit_none: "Keine Begrenzung",
    export_limit_kb: |kb| {
        if kb >= 1024 && kb % 1024 == 0 {
            format!("Bis zu {} MB", kb / 1024)
        } else {
            format!("Bis zu {kb} KB")
        }
    },
    export_jpeg_preset: "Kodierung",
    export_jpeg_fastest: "Schnellste",
    export_jpeg_baseline: "Baseline",
    export_jpeg_progressive: "Progressiv",
    export_jpeg_smallest: "Kleinste",
    export_jpeg_preset_hint: "„Schnellste“ kodiert um ein Vielfaches schneller. Die anderen erzeugen 10-20% kleinere Dateien.",
    export_naming: "Dateiname",
    export_naming_hint: "{name} ist der Name des Fotos und {seq} seine Nummer in diesem Export.",
    export_naming_example: |name| format!("Beispiel: {name}"),
    export_metadata: "Metadaten",
    export_metadata_include: "Einbeziehen",
    export_keep_location: "Ort (GPS)",
    export_keep_identity: "Seriennummern der Kamera und Eigentümer",
    export_keep_hint: "Aufnahmedatum, Kamera, Objektiv und Belichtung werden immer übernommen. Bildunterschriften und Kommentare nie.",
    export_copyright: "Copyright",
    export_copyright_hint: "Wird in jeden Export anstelle des Copyrights des Fotos geschrieben. Leer lassen, um das des Fotos zu behalten.",
    export_sharpen: "Schärfen für",
    export_sharpen_off: "Keine",
    export_sharpen_screen: "Bildschirm",
    export_sharpen_matte: "Mattes Papier",
    export_sharpen_glossy: "Glänzendes Papier",
    export_sharpen_low: "Niedrig",
    export_sharpen_standard: "Standard",
    export_sharpen_high: "Hoch",
    export_run: "Exportieren",
    export_needs_immich: "Verbinden Sie sich zuerst mit einem Immich-Server",
    immich_server_url: "Server-URL",
    immich_url_example: "Beispiel: https://immich.gumnut.ai",
    immich_plain_http: "http:// ist nicht verschlüsselt: Ihr API-Schlüssel und Ihre Fotos werden lesbar über das Netzwerk übertragen. Verwenden Sie https://, wenn der Server es anbietet.",
    immich_native_only: "Das Hochladen zu Immich ist nur in der Desktop-App möglich. Browser können sich nicht mit einem Immich-Server verbinden.",
    immich_api_key: "API-Schlüssel",
    immich_connect: "Verbinden",
    immich_connecting: "Verbindung wird hergestellt\u{2026}",
    immich_connected_as: |name| format!("Verbunden als {name}"),
    immich_disconnect: "Trennen",
    immich_key_storage: if cfg!(target_os = "macos") {
        "Der Schlüssel wird in Ihrem Schlüsselbund gespeichert."
    } else {
        "Der Schlüssel wird in einer Datei gespeichert, die nur Sie lesen können."
    },
    uploading: |done, total| format!("{done}/{total} werden hochgeladen\u{2026}"),
    uploaded: |n, dup| {
        let fotos = if n == 1 { "Foto" } else { "Fotos" };
        match dup {
            0 => format!("{n} {fotos} hochgeladen"),
            _ => format!("{n} {fotos} hochgeladen, {dup} schon auf dem Server"),
        }
    },
    uploaded_partial: |ok, total, e| format!("{ok}/{total} hochgeladen \u{2014} letzter Fehler: {e}"),
    ratings_not_set: |summary, n, e| format!("{summary} \u{2014} Bewertung bei {n} nicht gesetzt: {e}"),
    immich_album: "Album",
    album_none: "Kein Album",
    album_new: "Neues Album\u{2026}",
    album_name: "Name des Albums",
    album_name_needed: "Geben Sie dem neuen Album zuerst einen Namen",
    albums_failed: |e| format!("Alben konnten nicht geladen werden: {e}"),
    adding_to_album: |summary, name| format!("{summary} \u{2014} wird zu {name} hinzugefügt\u{2026}"),
    added_to_album: |summary, name| format!("{summary} \u{2014} zu {name} hinzugefügt"),
    album_failed: |summary, e| format!("{summary} \u{2014} nicht zum Album hinzugefügt: {e}"),
    export_nothing_selected: "Export: nichts ausgewählt",
    export_in_progress: "Ein Export läuft bereits\u{2026}",
    export_edits_loading: "Export: Die Bearbeitungen dieses Ordners werden noch geladen, versuchen Sie es gleich noch einmal\u{2026}",
    export_no_handle: "Export: kein Ordner-Handle für den aktuellen Ordner",
    export_no_folder: |e| format!("Export fehlgeschlagen: Der Ordner „Exports“ konnte nicht angelegt werden: {e}"),
    exporting: |done, total| format!("{done}/{total} werden exportiert\u{2026}"),
    exported: |n| {
        format!(
            "{n} {} exportiert",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    exported_partial: |ok, total, e| format!("{ok}/{total} exportiert \u{2014} letzter Fehler: {e}"),
    auto_tone_needs_load: "Für die Auto-Anpassung muss das Foto fertig geladen sein",
    auto_tone_applied: "Auto-Anpassung angewendet",
    remove_ca_measuring: "Chromatische Aberration wird gemessen…",
    remove_ca_measuring_n: |n, total| format!("Chromatische Aberration wird gemessen… {n} von {total}"),
    remove_ca_applied: "Chromatische Aberration entfernt",
    remove_ca_failed: "Das Foto ließ sich nicht dekodieren, um die chromatische Aberration zu messen",
    auto_tone_waits_for_edits: "Die Auto-Anpassung startet, sobald die Bearbeitungen dieses Ordners geladen sind\u{2026}",
    auto_tone_stopped: |done, total| format!("Auto-Anpassung bei {done}/{total} gestoppt"),
    auto_tone_applied_n: |n| {
        format!(
            "Auto-Anpassung auf {n} {} angewendet",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    auto_tone_progress: |done, total| format!("Auto-Anpassung {done}/{total}\u{2026}"),
    scoring_progress: |done, total| format!("Auswertung {done}/{total}\u{2026}"),
    scoring_stopped: |done, total| format!("Auswertung bei {done}/{total} gestoppt"),
    scored_n: |n| {
        format!(
            "{n} {} ausgewertet",
            if n == 1 { "Foto" } else { "Fotos" }
        )
    },
    scored_partial: |ok, total| format!("{ok}/{total} ausgewertet; der Rest ließ sich nicht lesen"),
    touch_up_limit: "Die Retusche unterstützt bis zu 64 Flecken",
    mask_limit: "Ein Foto kann bis zu 16 Masken enthalten",
    masks: "Masken",
    linear_gradient: "Linear",
    radial_gradient: "Radial",
    linear_gradient_tip: "Linearer Verlauf: auf dem Foto ziehen, um ihn zu zeichnen (M)",
    radial_gradient_tip: "Radialer Verlauf: auf dem Foto von der Mitte aus nach außen ziehen (Shift+M)",
    brush_mask: "Pinsel",
    brush_mask_tip: "Pinsel: die Maske auf das Foto malen (K). Mit Alt radieren",
    brush_paint_hint: "Auf dem Foto malen. Mit Alt radieren",
    flow: "Fluss",
    flow_tip: "Wie viel ein Durchgang aufträgt. Durchgänge über derselben Stelle summieren sich",
    brush_erase: "Radieren",
    brush_erase_tip: "Malen, um von der Maske abzuziehen, wie beim Halten von Alt",
    brush_point_limit: "Die Pinselstriche eines Fotos umfassen bis zu 8.000 Punkte",
    brush_stroke_limit: "Eine Pinselmaske umfasst bis zu 512 Striche; „+ Pinsel“ beginnt eine weitere",
    mask_invert: "Umkehren",
    mask_draw_hint: "Auf dem Foto ziehen, um die Maske zu zeichnen",
    subject_mask: "Motiv",
    sky_mask: "Himmel",
    detect_nothing_found: |what| format!("{what}: In diesem Foto nichts gefunden"),
    detect_failed: |what, why| format!("Erkennung fehlgeschlagen ({what}): {why}"),
    subject_mask_tip: "Motiv: eine Maske über der Person oder dem Hauptmotiv, für dieses Foto erkannt",
    sky_mask_tip: "Himmel: eine Maske über dem Himmel, für dieses Foto erkannt",
    detect_needs_mac: "Die Motiverkennung benötigt macOS",
    detect_unavailable: "In diesem Build noch nicht verfügbar",
    detect_file_missing: "Die Erkennungsdatei dieser Maske fehlt in .lightphotos/masks",
    detect_read_failed: |what, why| format!("Die Datei der Maske „{what}“ ließ sich nicht lesen: {why}"),
    mask_detecting: "Wird erkannt\u{2026}",
    mask_retry: "Erneut versuchen",
    export_masks_detecting: "Export: Die Masken werden noch erkannt, versuchen Sie es gleich noch einmal\u{2026}",
    touch_up_needs_full: "Die Retusche benötigt ein Bild in voller Auflösung",
    wb_no_image: "Kein Bild geladen, in dem gewählt werden kann",
    wb_pick_brighter: "Wählen Sie für den Weißabgleich ein helleres, weniger gesättigtes Pixel",
    open_folder_failed: |e| format!("Der Ordner konnte nicht geöffnet werden: {e}"),
    catalog_fallback: |e| format!("Der letzte Katalog konnte nicht geöffnet werden, daher ist der Standardkatalog geöffnet: {e}"),
    catalog_open_failed: |e| format!("Der Katalog konnte nicht geöffnet werden: {e}"),
    catalog_save_failed: |e| format!("Der Katalog konnte nicht gesichert werden: {e}"),
    preset_folders_unreadable: |e| format!("Die Vorgabenordner konnten nicht gelesen werden: {e}"),
    catalog_create_failed: |e| format!("Der Katalog konnte nicht angelegt werden: {e}"),
    catalog_tip: "Katalog: die hier aufgeführten Ordner und eine Kopie ihrer Bewertungen, Markierungen und Farbmarkierungen",
    new_catalog: "Neuer Katalog\u{2026}",
    open_catalog: "Katalog öffnen\u{2026}",
    catalog_save_title: "Neuer Katalog",
    catalog_untitled: "Unbenannt",
    catalog_filter_name: "LightPhotos-Katalog",
    catalog_picker_title: "Katalog wählen (ein .lightphotos-Ordner)",
    catalog_name_label: "Name",
    catalog_name_hint: "Name des Katalogs",
    create: "Anlegen",
    preview_failed: |path| format!("Die Vorschau der Lupenansicht für {path} konnte nicht geladen werden"),
    group_page: |first, last, n| format!("{first}–{last} von {n}"),
    tile_load: "Fotos laden:",
    tile_speed: "Schnell",
    tile_full: "Vollständig (langsamer)",
    set_as_rep: "Als Repräsentativfoto festlegen",
    representative: "Repräsentativfoto",
    view_compare: "Fotos vergleichen",
    view_compare_needs_group: "Fotos vergleichen: 2 bis 6 Fotos oder ein Foto in einem Stapel auswählen",
    folder_handle_missing: |dir| format!("{dir} konnte nicht geöffnet werden \u{2014} Ordner-Handle fehlt"),
    list_folder_failed: |dir, e| format!("{dir} konnte nicht aufgelistet werden: {e}"),
    edits_save_failed: |e| format!("Bewertungen und Bearbeitungen konnten nicht gesichert werden: {e}"),
    held_by_app: |pid| format!("ein anderes LightPhotos-Fenster (pid {pid})"),
    held_by_mcp: |pid| format!("lightphotos-mcp (pid {pid})"),
    held_by_other: "ein anderes Programm",
    folder_read_only: |who| {
        format!("Schreibgeschützt: {who} hat diesen Ordner geöffnet. Änderungen hier werden nicht gesichert. Schließen Sie ihn dort und öffnen Sie diesen Ordner dann erneut.")
    },
    folder_cant_lock: "Schreibgeschützt: Dieser Ordner kann zum Sichern nicht gesperrt werden, daher werden Änderungen hier nicht gesichert. Möglicherweise ist er schreibgeschützt oder erfordert eine Berechtigung. Öffnen Sie ihn erneut, um es noch einmal zu versuchen.",
    folder_no_access: "Schreibgeschützt: LightPhotos hat keinen Zugriff auf diesen Ordner, daher werden Änderungen hier nicht gesichert. Öffnen Sie ihn erneut.",
    folder_lockless: "Dieser Ordner kann nicht gesperrt werden. Lassen Sie ihn daher nicht von einem headless lightphotos-mcp ändern, solange er hier geöffnet ist.",
    folder_opening: "Dieser Ordner wird noch geöffnet. Versuchen Sie es gleich noch einmal.",
    presets_load_failed: |e| format!("Ihre Vorgaben konnten nicht gelesen werden: {e}"),
    presets_locked:
        "Die Vorgaben sind gesperrt, weil die gesicherte Datei nicht gelesen werden konnte. Verschieben oder reparieren Sie sie und starten Sie dann neu.",
    presets_save_failed: |e| format!("Die Vorgaben konnten nicht gesichert werden: {e}"),

    menu: MenuStrings {
        about: "Über LightPhotos",
        settings: "Einstellungen\u{2026}",
        hide: "LightPhotos ausblenden",
        hide_others: "Andere ausblenden",
        show_all: "Alle einblenden",
        quit: "LightPhotos beenden",
        file: "Ablage",
        new_catalog: "Neuer Katalog\u{2026}",
        open_catalog: "Katalog öffnen\u{2026}",
        open_folder: "Ordner öffnen\u{2026}",
        export: "Exportieren\u{2026}",
        edit: "Bearbeiten",
        undo: "Widerrufen",
        cut: "Ausschneiden",
        copy: "Kopieren",
        paste: "Einsetzen",
        select_all: "Alles auswählen",
        deselect_all: "Auswahl aufheben",
        copy_settings: "Anpassung kopieren…",
        paste_settings: "Anpassung einsetzen",
        photo: "Foto",
        rate: "Bewerten",
        no_rating: "Keine Bewertung",
        set_flag: "Markierung setzen",
        flag_picked: "Markiert",
        flag_unflagged: "Unmarkiert",
        flag_rejected: "Abgelehnt",
        color_label: "Farbmarkierung",
        label_red: "Rot",
        label_yellow: "Gelb",
        label_green: "Grün",
        label_blue: "Blau",
        label_purple: "Violett",
        auto_tone: "Auto-Anpassung",
        auto_tone_selection: "Auswahl automatisch anpassen",
        black_and_white: "Schwarzweiß",
        score_photos: "Qualität auswerten",
        rotate_left: "Nach links drehen",
        rotate_right: "Nach rechts drehen",
        move_to_trash: "In den Papierkorb legen",
        group_selected: "Stapeln",
        group_bursts: "Auto-Stapel nach Aufnahmezeit",
        ungroup: "Stapel auflösen",
        delete_group: "Stapel löschen\u{2026}",
        view: "Darstellung",
        grid: "Rasteransicht",
        loupe: "Lupenansicht",
        develop: "Anpassungen",
        crop: "Zuschneiden & Transformieren",
        compare_stack: "Fotos vergleichen",
        info_panel: "Infobereich",
        before_after: "Vorher / Nachher",
        toggle_zoom: "Zoom umschalten",
        zoom_in: "Vergrößern",
        zoom_out: "Verkleinern",
        bigger_text: "Größerer Text",
        smaller_text: "Kleinerer Text",
        window: "Fenster",
        minimize: "Im Dock ablegen",
        zoom: "Zoomen",
        help: "Hilfe",
        keyboard_shortcuts: "Tastaturkurzbefehle",
    },
};
