use bens_chat_shared::Bot;
fn main() {
    Bot::no_responce(|msg| {
        println!("msg: {}", msg.content);
    })
}
