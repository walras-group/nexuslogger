import nexuslog as logging
import time

logging.basicConfig(level=logging.DEBUG, unix_ts=True, filename='tmp/app.log')

class A:
    log = logging.getLogger()

class B:
    log = logging.getLogger()

def main():
    a = A()
    b = B()

    for i in range(1000):
        a.log.info(f"Message {i} from A")
        b.log.debug(f"Message {i} from B")

    logging.shutdown()

if __name__ == "__main__":
    main()
