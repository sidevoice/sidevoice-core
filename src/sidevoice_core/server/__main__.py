"""`python -m sidevoice_core.server` (or `sidevoice-core`): serve this node on loopback."""
import argparse


def main(argv=None):
    import uvicorn
    from .app import create_app
    parser = argparse.ArgumentParser(prog='sidevoice-core',
                                     description="Sidevoice core: this node's conversations and one voice pipeline per call.")
    parser.add_argument('--host', default='127.0.0.1')
    parser.add_argument('--port', type=int, default=8767)
    arguments = parser.parse_args(argv)
    uvicorn.run(create_app(), host=arguments.host, port=arguments.port)


if __name__ == '__main__':
    main()
