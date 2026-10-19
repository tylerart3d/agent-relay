import base64
import io
import unittest
from PIL import Image
from server import decode_crops


def image_data(size=(112, 112)):
    stream = io.BytesIO()
    Image.new("RGB", size, (70, 140, 210)).save(stream, format="PNG")
    return base64.b64encode(stream.getvalue()).decode()


class CropTests(unittest.TestCase):
    def test_aligned_rgb_crop(self):
        result = decode_crops({"images": [image_data()]})
        self.assertEqual(result[0].getpixel((0, 0)), (70, 140, 210))

    def test_rejects_bad_inputs(self):
        for payload in [{}, {"images": []}, {"images": ["invalid!"]},
                        {"images": [image_data((224, 224))]},
                        {"images": [image_data()] * 17},
                        {"images": [image_data()], "url": "http://example.com"}]:
            with self.assertRaises(ValueError):
                decode_crops(payload)


if __name__ == "__main__":
    unittest.main()
