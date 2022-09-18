use yew_agent::PublicWorker;

use fuzzysearch_web::workers::ImageHasherWorker;

fn main() {
    ImageHasherWorker::register();
}
