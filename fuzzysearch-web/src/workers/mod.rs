use serde::{Deserialize, Serialize};
use yew_agent::{HandlerId, Public, Worker, WorkerLink};

#[derive(Debug, Serialize, Deserialize)]
pub struct ImageHasherWorkerInput {
    pub contents: Vec<u8>,
}

#[derive(Debug, Serialize, Deserialize)]
pub enum ImageHasherWorkerOutput {
    Starting,
    Finished { hash: Option<i64> },
}

pub struct ImageHasherWorker {
    link: WorkerLink<Self>,
}

impl Worker for ImageHasherWorker {
    type Input = ImageHasherWorkerInput;
    type Message = ();
    type Output = ImageHasherWorkerOutput;
    type Reach = Public<Self>;

    fn create(link: WorkerLink<Self>) -> Self {
        Self { link }
    }

    fn update(&mut self, _msg: Self::Message) {}

    fn handle_input(&mut self, msg: Self::Input, id: HandlerId) {
        self.link.respond(id, ImageHasherWorkerOutput::Starting);

        let hash = image::load_from_memory(&msg.contents)
            .ok()
            .map(|im| Self::get_hasher().hash_image(&im))
            .and_then(|hash| hash.as_bytes().try_into().ok())
            .map(i64::from_be_bytes);

        self.link
            .respond(id, ImageHasherWorkerOutput::Finished { hash });
    }

    fn name_of_resource() -> &'static str {
        "worker.js"
    }
}

impl ImageHasherWorker {
    fn get_hasher() -> img_hash::Hasher<[u8; 8]> {
        use img_hash::HashAlg;

        img_hash::HasherConfig::with_bytes_type::<[u8; 8]>()
            .hash_alg(HashAlg::Gradient)
            .hash_size(8, 8)
            .preproc_dct()
            .to_hasher()
    }
}
