use nth_curios::{MAX_HTML_BYTES, validate};

#[test]
fn embedded_raster_mime_labels_do_not_substitute_for_validating_image_bytes() {
    // "bm90IGFuIGltYWdl" decodes to "not an image". A declared MIME type
    // alone does not establish the embedded raster-image contract.
    for mime in ["png", "jpeg", "gif", "webp"] {
        let html = format!("<img src='data:image/{mime};base64,bm90IGFuIGltYWdl'>");
        assert!(validate(&html).is_err(), "accepted non-image bytes: {html}");
    }
}

#[test]
fn css_and_svg_external_resource_forms_are_rejected() {
    let inputs = [
        "<p style='background-image:image-set(\"https://evil.invalid/a.png\" 1x)'>x</p>",
        "<style>@font-face{font-family:x;src:url(https://evil.invalid/x)}</style>",
        "<style>@import/**/'https://evil.invalid/x';</style>",
        "<svg><use href='//evil.invalid/x.svg#id'/></svg>",
        "<svg><image href='data:image/svg+xml,%3Csvg%3E'/></svg>",
        "<svg><rect fill='url(https://evil.invalid/x.svg#id)'/></svg>",
        "<svg><path marker-end='url(https://evil.invalid/x.svg#arrow)'/></svg>",
        "<svg><path marker-start='url(https://evil.invalid/x.svg#arrow)'/></svg>",
        "<svg><path marker-mid='url(https://evil.invalid/x.svg#arrow)'/></svg>",
        "<svg><rect fill='u\\72l(https://evil.invalid/x.svg#id)'/></svg>",
        "<svg><rect fill='url(#local) url(https://evil.invalid/x.svg#id)'/></svg>",
    ];
    let accepted: Vec<_> = inputs
        .into_iter()
        .filter(|html| validate(html).is_ok())
        .collect();
    assert!(
        accepted.is_empty(),
        "accepted external resource forms: {accepted:#?}"
    );
}

#[test]
fn active_navigation_and_parser_normalized_attributes_are_rejected() {
    for html in [
        "<a href='java&#x73;cript:alert(1)'>Go</a>",
        "<img srcset='https://evil.invalid/x 1x'>",
        "<svg><a href='https://evil.invalid'>Go</a></svg>",
        "<svg><foreignObject><p>Embedded</p></foreignObject></svg>",
        "<svg><set attributeName='href' to='https://evil.invalid'/></svg>",
        "<meta HTTP-EQUIV='refresh' content='0;url=https://evil.invalid'>",
        "<div ONPOINTERENTER='alert(1)'>Go</div>",
        "<base href='https://evil.invalid/'>",
        "<img src='file:///etc/passwd'>",
        "<a href='/api/state'>API</a>",
    ] {
        assert!(validate(html).is_err(), "accepted active content: {html}");
    }
}

#[test]
fn static_limits_duplicate_ids_and_safe_local_svg() {
    assert!(validate(&"x".repeat(MAX_HTML_BYTES + 1)).is_err());
    assert!(validate("<p>before\0after</p>").is_err());
    assert!(validate("<h1 id='a'>A</h1><svg><rect id='a'/></svg>").is_err());
    assert!(validate(&format!("<p id='{}'>x</p>", "x".repeat(161))).is_err());
    let too_many: String = (0..4097).map(|i| format!("<p id='a{i}'>x</p>")).collect();
    assert!(validate(&too_many).is_err());
    let anchors = validate("<style>@media(max-width:600px){p{color:red}}</style><svg><defs><linearGradient id='g'/></defs><rect id='box' fill='url(#g)'/></svg><p id='text'>A &amp; B</p>").unwrap();
    assert!(anchors.iter().any(|a| a.id == "text" && a.label == "A & B"));
}

#[test]
fn the_word_url_is_text_and_local_css_references_are_allowed() {
    for html in [
        "<img alt='URL routing diagram'>",
        "<span title='Paste the URL'>x</span>",
        "<p class='curly'>x</p>",
        "<style>.surly{color:red}</style>",
        "<style>circle{fill:url(#g)}</style><svg><circle id='c' r='1'/></svg>",
        "<p style='mask:URL(#m)'>x</p>",
    ] {
        assert!(validate(html).is_ok(), "rejected {html}");
    }
    for html in [
        "<style>a{background:url(http://evil.invalid/x)}</style>",
        "<style>a{background:url(#a), URL(x)}</style>",
        "<p style='fill:url(#a) url(x)'>x</p>",
        "<style>a{background:url('#a')}</style>",
    ] {
        assert!(validate(html).is_err(), "accepted {html}");
    }
}
