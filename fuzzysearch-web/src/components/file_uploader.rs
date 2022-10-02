use std::collections::HashMap;

use gloo::{
    events::EventListener,
    file::{callbacks::FileReader, File},
};
use semester::StaticClasses;
use wasm_bindgen::JsCast;
use web_sys::{FileList, HtmlElement, HtmlInputElement};
use yew::prelude::*;

use crate::umami;

#[derive(Default)]
enum FileUploaderDragState {
    #[default]
    None,
    Valid,
}

#[derive(Default)]
pub struct FileUploader {
    readers: HashMap<String, FileReader>,
    preview_url: Option<String>,
    drag_state: FileUploaderDragState,
    upload_container_ref: NodeRef,
    #[allow(dead_code)]
    paste_listener: Option<EventListener>,
}

pub enum FileUploaderMsg {
    Files(Vec<File>),
    GotPreviewUrl(Option<String>),
    Loaded(String, String, Option<Vec<u8>>),
    DragEvent(DragEvent),
    UploadContainerClicked,
    Paste(Event),
}

#[derive(Properties, PartialEq)]
pub struct FileUploaderProps {
    pub on_file_upload: Callback<Vec<u8>>,
}

impl Component for FileUploader {
    type Message = FileUploaderMsg;
    type Properties = FileUploaderProps;

    fn create(ctx: &Context<Self>) -> Self {
        let onpaste = ctx.link().callback(FileUploaderMsg::Paste);

        let paste_listener = web_sys::window().map(|window| {
            umami::track_event("image-search", "paste");
            EventListener::new(&window, "paste", move |event| onpaste.emit(event.clone()))
        });

        Self {
            paste_listener,
            ..Default::default()
        }
    }

    fn update(&mut self, ctx: &Context<Self>, msg: Self::Message) -> bool {
        match msg {
            FileUploaderMsg::Files(files) => {
                tracing::debug!("got {} files", files.len());

                if let Some(input_element) = self.upload_container_ref.cast::<HtmlInputElement>() {
                    input_element.set_value("");
                }

                for file in files {
                    let file_name = file.name();
                    let mime_type = file.raw_mime_type();

                    let preview_url = web_sys::Url::create_object_url_with_blob(file.as_ref()).ok();
                    ctx.link()
                        .send_message(FileUploaderMsg::GotPreviewUrl(preview_url));

                    let task = {
                        let link = ctx.link().clone();
                        let file_name = file_name.clone();

                        gloo::file::callbacks::read_as_bytes(&file, move |res| {
                            link.send_message(FileUploaderMsg::Loaded(
                                file_name,
                                mime_type,
                                res.ok(),
                            ))
                        })
                    };
                    self.readers.insert(file_name, task);
                }

                self.drag_state = FileUploaderDragState::None;
                true
            }
            FileUploaderMsg::GotPreviewUrl(url) => {
                self.preview_url = url;
                true
            }
            FileUploaderMsg::Loaded(name, mime, contents) => {
                tracing::info!(mime_type = mime, "loaded file {name}");

                self.readers.remove(&name);

                if let Some(contents) = contents {
                    ctx.props().on_file_upload.emit(contents);
                }

                true
            }
            FileUploaderMsg::DragEvent(event) => {
                event.prevent_default();

                match event.type_().as_str() {
                    "dragstart" | "dragover" => {
                        self.drag_state = FileUploaderDragState::Valid;
                        true
                    }
                    "dragleave" => {
                        self.drag_state = FileUploaderDragState::None;
                        true
                    }
                    other_event => {
                        tracing::warn!("got unexpected drag event: {other_event}");
                        false
                    }
                }
            }
            FileUploaderMsg::UploadContainerClicked => {
                if let Some(html_element) = self.upload_container_ref.cast::<HtmlElement>() {
                    html_element.click();
                }

                false
            }
            FileUploaderMsg::Paste(paste) => {
                if let Some(data) = paste
                    .dyn_into::<web_sys::ClipboardEvent>()
                    .ok()
                    .and_then(|event| event.clipboard_data())
                {
                    ctx.link().send_message(Self::upload_files(data.files()));
                }

                false
            }
        }
    }

    fn view(&self, ctx: &Context<Self>) -> Html {
        let drag_event = ctx.link().callback(FileUploaderMsg::DragEvent);

        let styles = self
            .preview_url
            .as_deref()
            .map(|url| format!("background-image: url({})", url));

        html! {
            <form class="image-uploader">
                <div
                    class={self.classes()}
                    style={styles}
                    ondrop={ctx.link().callback(|event: DragEvent| {
                        event.prevent_default();
                        let files = event.data_transfer().map(|dt| dt.files()).unwrap_or_default();
                        umami::track_event("image-search", "drop");
                        Self::upload_files(files)
                    })}
                    onclick={ctx.link().callback(|event: MouseEvent| {
                        event.prevent_default();
                        umami::track_event("image-search", "upload");
                        FileUploaderMsg::UploadContainerClicked
                    })}
                    ondragover={drag_event.clone()}
                    ondragstart={drag_event.clone()}
                    ondragleave={drag_event.clone()}
                >
                    <p>{ "Upload or Drop Here" }</p>
                </div>

                <input
                    id="file-upload"
                    type="file"
                    accept="image/jpeg, image/png, image/webp"
                    ref={self.upload_container_ref.clone()}
                    onchange={ctx.link().callback(move |e: Event| {
                        let input: HtmlInputElement = e.target_unchecked_into();
                        umami::track_event("image-search", "upload");
                        Self::upload_files(input.files())
                    })} />
            </form>
        }
    }
}

impl FileUploader {
    fn classes(&self) -> &'static str {
        semester::static_classes!(
            "file-upload",
            "drag-valid": matches!(self.drag_state, FileUploaderDragState::Valid),
            "previewing": self.preview_url.is_some()
        )
        .as_str()
    }

    fn upload_files(files: Option<FileList>) -> FileUploaderMsg {
        let mut results = Vec::with_capacity(
            files
                .as_ref()
                .map(|files| files.length())
                .unwrap_or_default() as usize,
        );

        if let Some(files) = files {
            let files: Vec<File> = js_sys::try_iter(&files)
                .ok()
                .flatten()
                .map(|files| {
                    files
                        .filter_map(|file| file.ok())
                        .map(web_sys::File::from)
                        .map(From::from)
                        .collect()
                })
                .unwrap_or_default();

            results.extend(files);
        }

        FileUploaderMsg::Files(results)
    }
}
